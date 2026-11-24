#!/usr/bin/env python3
"""把 `/api/audit/export` 的导出一路送进 OpenSSH 签名、锚定链与 WORM 对象库。

这条管线只做编排，不复制 adapter 的逻辑：

- 签名：``adapters/audit/sign_export.py``（``ssh-keygen -Y sign``）
- 锚定：``adapters/audit/anchor.py``（``anchors.jsonl`` 哈希链）
- 对象库：``adapters/worm/store.py``（内容寻址、保留期内拒绝删除）

产物（``--out-dir`` + ``--name``）：

    <name>.json        原始导出字节（逐字节保存，不重新格式化）
    <name>.json.sig    OpenSSH detached 签名
    anchors.jsonl      追加一条 {"seq","sha256","prev","ts"}（路径由 --anchors 指定）
    <store>/objects/   导出 JSON 作为内容寻址对象（retention 由参数决定）

顺序：先在同目录 staging 里落盘、签名、验签，再预检 anchor 链与 store，
然后做**前向不可逆**的 store put / anchor append，最后原子提交产物并整体复核。
任何一步失败都以非零退出，不把 staging 当成品留下；store 是 WORM 风格、
put 不可回滚，失败时对象可能已经存在（见 docs/54 的边界说明）。

安全约定：

- token / 私钥内容不进日志、不进输出；``--token`` 推荐改用 ``--token-file`` 或
  环境变量 ``UI_AUDIT_TOKEN``（命令行参数可能被同机进程看到）；
- URL 模式要求 HTTP 200 且响应体是可解析 JSON；不打印 URL、响应头或 token；
- 临时文件放在输出目录下的 staging 目录，成功提交后删除，失败时清理。
"""
from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import re
import shutil
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from adapters.audit import anchor as anchor_mod  # noqa: E402
from adapters.audit import sign_export as sign_mod  # noqa: E402
from adapters.worm import store as worm_store  # noqa: E402

EXIT_OK = 0
EXIT_FAIL = 1
EXIT_SKIP = 2
EXIT_USAGE = 64

MAX_EXPORT_BYTES = 64 * 1024 * 1024
HEX_RE = re.compile(r"^[0-9a-f]{64}$")
NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


class PipelineError(Exception):
    """管线失败（退出码 1）。"""

    exit_code = EXIT_FAIL


class UsageError(PipelineError):
    """参数/用法错误（退出码 64）。"""

    exit_code = EXIT_USAGE


class Parser(argparse.ArgumentParser):
    # argparse 默认用 2 表示用法错误，会和 SKIP 的 2 混淆。
    def error(self, message: str) -> None:  # type: ignore[override]
        self.print_usage(sys.stderr)
        self.exit(EXIT_USAGE, f"{self.prog}: error: {message}\n")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def parse_export(data: bytes, label: str) -> dict:
    if not data:
        raise PipelineError(f"{label} is empty")
    try:
        obj = json.loads(data)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PipelineError(f"{label} is not valid JSON: {exc}") from None
    if not isinstance(obj, dict):
        raise PipelineError(f"{label} must be a JSON object")
    events = obj.get("events")
    if events is not None and not isinstance(events, list):
        raise PipelineError(f"{label}: 'events' must be a JSON array")
    return obj


def resolve_token(args: argparse.Namespace) -> str | None:
    """按 --token > --token-file > UI_AUDIT_TOKEN 取值，绝不打印。"""
    if args.token and args.token_file:
        raise UsageError("give only one of --token and --token-file")
    if args.token_file:
        path = Path(args.token_file).expanduser()
        try:
            token = path.read_text(encoding="utf-8").strip()
        except OSError as exc:
            raise PipelineError(f"cannot read --token-file: {exc}") from None
    elif args.token:
        token = args.token
    else:
        token = os.environ.get("UI_AUDIT_TOKEN", "")
    if not token:
        return None
    if "\n" in token or "\r" in token:
        raise UsageError("token must not contain newlines")
    return token


def fetch_export(url: str, token: str | None, timeout: float) -> bytes:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme not in ("http", "https"):
        raise UsageError("--url must use http:// or https://")
    if parts.username or parts.password:
        raise UsageError("--url must not contain credentials; use --token/--token-file")
    if timeout <= 0:
        raise UsageError("--url-timeout must be > 0")
    headers = {"Accept": "application/json", "User-Agent": "ui-audit-worm-pipeline/1"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            status = getattr(resp, "status", None)
            if status is None:
                status = resp.getcode()
            if status != 200:
                raise PipelineError(f"GET failed: HTTP {status}")
            data = resp.read(MAX_EXPORT_BYTES + 1)
    except urllib.error.HTTPError as exc:
        # 不回显 URL / 响应头，避免把查询串或 token 带进日志。
        raise PipelineError(f"GET failed: HTTP {exc.code} {exc.reason}") from None
    except urllib.error.URLError as exc:
        raise PipelineError(f"GET failed: {exc.reason}") from None
    except http.client.HTTPException as exc:
        raise PipelineError(f"GET failed: incomplete/invalid HTTP response: {exc}") from None
    except (OSError, ValueError) as exc:
        raise PipelineError(f"GET failed: {exc}") from None
    if len(data) > MAX_EXPORT_BYTES:
        raise PipelineError(
            f"GET response exceeds {MAX_EXPORT_BYTES} bytes; refusing to buffer it"
        )
    return data


def pick_name(args: argparse.Namespace) -> str:
    if args.name:
        name = args.name
    elif args.export is not None:
        name = Path(args.export).stem or "export"
    else:
        name = "audit-export-" + time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    if name.endswith(".") or not NAME_RE.match(name):
        raise UsageError(
            "--name/derived name must match [A-Za-z0-9][A-Za-z0-9._-]* "
            f"(got {name!r})"
        )
    return name


def resolve_retention(args: argparse.Namespace) -> int:
    if args.retention_until is not None:
        retention = args.retention_until
        if retention < 0:
            raise UsageError("--retention-until must be >= 0 (epoch seconds)")
    else:
        days = args.retention_days
        if days is None or days < 0:
            raise UsageError("--retention-days must be >= 0")
        retention = int(time.time()) + days * 86400
    if retention < int(time.time()):
        print(
            f"WARN: retentionUntil={retention} is already in the past; "
            "the object could be deleted immediately",
            file=sys.stderr,
        )
    return retention


def check_signer_pair(args: argparse.Namespace) -> None:
    if bool(args.allowed_signers) != bool(args.identity):
        raise UsageError("--allowed-signers and --identity must be given together")


def read_anchor_entries(path: Path, must_exist: bool = False) -> list[dict]:
    if not path.exists():
        if must_exist:
            raise PipelineError(f"anchors file not found: {path}")
        return []
    if not path.is_file():
        raise PipelineError(f"anchors path is not a file: {path}")
    try:
        entries, torn = anchor_mod.read_entries(path)
    except anchor_mod.AnchorsError as exc:
        raise PipelineError(f"anchors: {exc}") from None
    if torn:
        raise PipelineError(
            f"anchors file has a torn tail: {path} (refusing to extend a broken chain)"
        )
    try:
        anchor_mod.check_chain(entries)
    except anchor_mod.AnchorsError as exc:
        raise PipelineError(f"anchors: {exc}") from None
    return entries


def preflight_anchor(path: Path) -> list[dict]:
    entries = read_anchor_entries(path)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise PipelineError(f"cannot create anchors directory {path.parent}: {exc}") from None
    return entries


def preflight_store(store_path: Path) -> None:
    if not store_path.exists():
        return
    if not store_path.is_dir():
        raise PipelineError(f"store path is not a directory: {store_path}")
    report = worm_store.WormStore(store_path).verify()
    if not report["ok"]:
        detail = "; ".join(report["errors"][:3]) or "(no detail)"
        raise PipelineError(f"store preflight failed: {detail}")


def append_anchor(path: Path, digest: str, entries: list[dict]) -> dict:
    """复用 anchor.cmd_append；同一份导出已是最后一条时不再重复追加。"""
    if entries and entries[-1]["sha256"] == digest:
        last = entries[-1]
        print(
            f"OK anchor  already at seq={last['seq']} sha256={digest} "
            f"(no duplicate append)"
        )
        return last
    ns = argparse.Namespace(anchors=str(path), export=None, sha256=digest, ts=None)
    anchor_mod.cmd_append(ns)
    fresh = read_anchor_entries(path, must_exist=True)
    if not fresh or fresh[-1]["sha256"] != digest:
        raise PipelineError("anchor append did not produce the expected last record")
    return fresh[-1]


def put_object(store_path: Path, data: bytes, retention: int, digest: str) -> dict:
    store = worm_store.WormStore(store_path)
    oid = store.put(data, retention)
    if oid != digest:
        raise PipelineError(
            f"store put returned id {oid}, expected export sha256 {digest}"
        )
    if store.get(oid) != data:
        raise PipelineError("stored object bytes differ from the export")
    report = store.verify()
    if not report["ok"]:
        detail = "; ".join(report["errors"][:3]) or "(no detail)"
        raise PipelineError(f"store verification failed after put: {detail}")
    print(
        f"OK worm    id={oid} bytes={len(data)} retentionUntil={retention} "
        f"records={report['records']}"
    )
    return report


def verify_anchor_boundary(
    path: Path, expect_head: str | None, expect_count: int | None
) -> None:
    if expect_head is None and expect_count is None:
        return
    ns = argparse.Namespace(
        anchors=str(path),
        expect_head=expect_head,
        expect_count=expect_count,
        json=False,
    )
    code = anchor_mod.cmd_verify(ns)
    if code != anchor_mod.EXIT_OK:
        raise PipelineError(f"anchor verification failed (exit {code})")


def verify_bundle(
    export_path: Path,
    sig_path: Path,
    anchors_path: Path,
    store_path: Path,
    *,
    object_id: str | None = None,
    expected_seq: int | None = None,
    allowed_signers: str | None = None,
    identity: str | None = None,
    namespace: str = sign_mod.DEFAULT_NAMESPACE,
    expect_head: str | None = None,
    expect_count: int | None = None,
) -> dict:
    """校验导出字节、WORM 对象、锚定链与（可选）OpenSSH 签名。"""
    if bool(allowed_signers) != bool(identity):
        raise UsageError("--allowed-signers and --identity must be given together")
    if not export_path.is_file():
        raise PipelineError(f"export not found: {export_path}")
    data = export_path.read_bytes()
    digest = sha256_bytes(data)
    parse_export(data, f"export {export_path}")

    want_id = digest
    if object_id:
        if not HEX_RE.match(object_id):
            raise UsageError("--object-id must be 64 lowercase hex chars")
        if object_id != digest:
            raise PipelineError(
                f"--object-id {object_id} does not match export sha256 {digest}"
            )
        want_id = object_id

    store = worm_store.WormStore(store_path)
    got = store.get(want_id)
    if got != data:
        raise PipelineError(f"store object {want_id} content differs from the export")
    report = store.verify()
    if not report["ok"]:
        detail = "; ".join(report["errors"][:3]) or "(no detail)"
        raise PipelineError(f"store verification failed: {detail}")

    if not sig_path.is_file():
        raise PipelineError(f"signature not found: {sig_path}")
    if sig_path.stat().st_size == 0:
        raise PipelineError(f"signature is empty: {sig_path}")

    entries = read_anchor_entries(anchors_path, must_exist=True)
    if expected_seq is not None:
        if expected_seq < 1:
            raise UsageError("--expected-seq must be >= 1")
        entry = next((e for e in entries if e["seq"] == expected_seq), None)
        if entry is None or entry["sha256"] != digest:
            raise PipelineError(
                f"anchor seq {expected_seq} does not record export sha256 {digest}"
            )
        anchor_seq = expected_seq
    else:
        matches = [e for e in entries if e["sha256"] == digest]
        if not matches:
            raise PipelineError(
                f"export sha256 {digest} is not recorded in {anchors_path}"
            )
        anchor_seq = matches[-1]["seq"]
    hashes = anchor_mod.check_chain(entries)
    head = hashes[-1] if hashes else anchor_mod.ZERO
    verify_anchor_boundary(anchors_path, expect_head, expect_count)

    signature_state = "not checked (no --allowed-signers)"
    if allowed_signers:
        exe = sign_mod.find_ssh_keygen()
        sign_mod.require_sign_support(exe)
        sign_mod.run_verify(
            exe,
            export_path,
            sig_path,
            Path(allowed_signers).expanduser(),
            identity or "",
            namespace,
        )
        signature_state = "verified"
        print(
            f"OK sig     {sig_path} verified for identity={identity} "
            f"namespace={namespace}"
        )

    print(
        f"OK verify  export={export_path} sha256={digest} object={want_id} "
        f"anchorSeq={anchor_seq} anchorHead={head} storeRecords={report['records']} "
        f"signature={signature_state}"
    )
    return {
        "sha256": digest,
        "object": want_id,
        "anchorSeq": anchor_seq,
        "anchorHead": head,
        "signature": signature_state,
    }


def cmd_run(args: argparse.Namespace) -> int:
    check_signer_pair(args)
    # 先探测能力：没有 OpenSSH -Y sign 时明确 SKIP，不降级成“没签也算成功”。
    exe = sign_mod.find_ssh_keygen()
    sign_mod.require_sign_support(exe)
    key = Path(args.key).expanduser()
    if not key.is_file():
        raise PipelineError(f"key not found: {key}")

    if args.export is not None:
        source = Path(args.export).expanduser()
        if not source.is_file():
            raise PipelineError(f"export not found: {source}")
        try:
            if source.stat().st_size > MAX_EXPORT_BYTES:
                raise PipelineError(
                    f"export exceeds {MAX_EXPORT_BYTES} bytes; refusing to buffer it"
                )
            data = source.read_bytes()
        except OSError as exc:
            raise PipelineError(f"cannot read export: {exc}") from None
    else:
        token = resolve_token(args)
        data = fetch_export(args.url, token, args.url_timeout)
    parse_export(data, "export")
    digest = sha256_bytes(data)

    name = pick_name(args)
    out_dir = Path(args.out_dir).expanduser()
    created_out_dir = not out_dir.exists()
    try:
        out_dir.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise PipelineError(f"cannot create --out-dir {out_dir}: {exc}") from None
    json_path = out_dir / f"{name}.json"
    sig_path = out_dir / f"{name}.json.sig"
    if (
        args.export is not None
        and json_path.resolve() == Path(args.export).expanduser().resolve()
    ):
        raise PipelineError(
            "--out-dir/--name would overwrite the input export; choose another name"
        )
    anchors = Path(args.anchors).expanduser()
    if anchors.resolve() in (json_path.resolve(), sig_path.resolve()):
        raise PipelineError("--anchors must not point at an output artifact")
    if json_path.exists() or sig_path.exists():
        raise PipelineError(
            f"output already exists: {json_path if json_path.exists() else sig_path} "
            "(refusing to overwrite audit artifacts)"
        )

    retention = resolve_retention(args)
    store_path = Path(args.store).expanduser()
    entries = preflight_anchor(anchors)
    preflight_store(store_path)

    stage = Path(tempfile.mkdtemp(prefix=f".{name}.", suffix=".tmp", dir=str(out_dir)))
    staged_json = stage / f"{name}.json"
    staged_sig = Path(str(staged_json) + ".sig")
    created: list[Path] = []
    try:
        sign_mod.atomic_write(staged_json, data)
        sign_ns = argparse.Namespace(
            export=str(staged_json),
            key=str(key),
            sig=None,
            namespace=args.namespace,
            identity=args.identity,
            allowed_signers=args.allowed_signers,
            force=False,
        )
        sign_mod.cmd_sign(sign_ns)
        if args.allowed_signers:
            sign_mod.run_verify(
                exe,
                staged_json,
                staged_sig,
                Path(args.allowed_signers).expanduser(),
                args.identity,
                args.namespace,
            )
            print("OK sig     staging signature verified")

        put_object(store_path, data, retention, digest)
        anchor_entry = append_anchor(anchors, digest, entries)
        if anchor_entry["sha256"] != digest:
            raise PipelineError("anchor record does not name the export sha256")
        verify_anchor_boundary(anchors, args.expect_head, args.expect_count)

        if json_path.exists() or sig_path.exists():
            raise PipelineError(
                f"output appeared while the pipeline was running: "
                f"{json_path if json_path.exists() else sig_path}"
            )
        os.replace(staged_json, json_path)
        created.append(json_path)
        os.replace(staged_sig, sig_path)
        created.append(sig_path)

        summary = verify_bundle(
            json_path,
            sig_path,
            anchors,
            store_path,
            object_id=digest,
            expected_seq=anchor_entry["seq"],
            allowed_signers=args.allowed_signers,
            identity=args.identity,
            namespace=args.namespace,
            expect_head=args.expect_head,
            expect_count=args.expect_count,
        )
    except BaseException:
        for path in reversed(created):
            try:
                path.unlink()
            except OSError:
                pass
        raise
    finally:
        shutil.rmtree(stage, ignore_errors=True)
        if created_out_dir:
            try:
                out_dir.rmdir()
            except OSError:
                pass

    print(
        f"PASS audit->worm pipeline: name={name} sha256={digest} "
        f"object={summary['object']} anchorSeq={summary['anchorSeq']} "
        f"anchorHead={summary['anchorHead']} signature={summary['signature']}"
    )
    return EXIT_OK


def cmd_verify(args: argparse.Namespace) -> int:
    export = Path(args.export).expanduser()
    sig = (
        Path(args.sig).expanduser()
        if args.sig
        else Path(str(export) + ".sig")
    )
    verify_bundle(
        export,
        sig,
        Path(args.anchors).expanduser(),
        Path(args.store).expanduser(),
        object_id=args.object_id,
        expected_seq=args.expected_seq,
        allowed_signers=args.allowed_signers,
        identity=args.identity,
        namespace=args.namespace,
        expect_head=args.expect_head,
        expect_count=args.expect_count,
    )
    print(f"PASS audit->worm verify: export={export}")
    return EXIT_OK


def build_parser() -> Parser:
    p = Parser(
        prog="audit_export_to_worm.py",
        description=(
            "export -> OpenSSH signature -> append-only anchors -> local WORM-style "
            "store (L2; NOT S3 Object Lock)"
        ),
    )
    sub = p.add_subparsers(dest="command", required=True)

    r = sub.add_parser("run", help="run the whole pipeline")
    src = r.add_mutually_exclusive_group(required=True)
    src.add_argument("--export", metavar="JSON", help="path to an already-saved export")
    src.add_argument("--url", metavar="URL", help="GET an /api/audit/export URL")
    r.add_argument(
        "--token",
        help="bearer token for --url (prefer --token-file or UI_AUDIT_TOKEN)",
    )
    r.add_argument("--token-file", help="file containing the bearer token")
    r.add_argument("--url-timeout", type=float, default=30.0, help="HTTP timeout seconds")
    r.add_argument("--out-dir", default=".", help="directory for <name>.json[.sig]")
    r.add_argument("--name", help="artifact base name (default: export stem or UTC time)")
    r.add_argument("--key", required=True, help="OpenSSH private key used to sign")
    r.add_argument("--anchors", required=True, help="anchors.jsonl to append to")
    r.add_argument("--store", required=True, help="WORM-style store root")
    ret = r.add_mutually_exclusive_group(required=True)
    ret.add_argument("--retention-days", type=int, help="retention end = now + N days")
    ret.add_argument(
        "--retention-until", type=int, help="retention end (epoch seconds)"
    )
    r.add_argument(
        "--allowed-signers",
        help="optional: verify the signature after signing (needs --identity)",
    )
    r.add_argument("--identity", help="signer identity for --allowed-signers")
    r.add_argument("--namespace", default=sign_mod.DEFAULT_NAMESPACE)
    r.add_argument(
        "--expect-head",
        help="optional: known anchor head; if missing, the chain was truncated",
    )
    r.add_argument(
        "--expect-count",
        type=int,
        help="optional: known anchor count; fewer entries means truncation",
    )
    r.set_defaults(func=cmd_run)

    v = sub.add_parser("verify", help="verify an already-produced artifact set")
    v.add_argument("--export", required=True)
    v.add_argument("--sig", help="default: <export>.sig")
    v.add_argument("--anchors", required=True)
    v.add_argument("--store", required=True)
    v.add_argument(
        "--object-id",
        help="expected WORM object id (default: sha256 of the export bytes)",
    )
    v.add_argument(
        "--expected-seq",
        type=int,
        help="require this anchor seq to record the export sha256",
    )
    v.add_argument("--allowed-signers")
    v.add_argument("--identity")
    v.add_argument("--namespace", default=sign_mod.DEFAULT_NAMESPACE)
    v.add_argument("--expect-head")
    v.add_argument("--expect-count", type=int)
    v.set_defaults(func=cmd_verify)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except sign_mod.Skip as exc:
        print(f"SKIP: {exc}", file=sys.stderr)
        return EXIT_SKIP
    except PipelineError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return exc.exit_code
    except (sign_mod.Fail, anchor_mod.AnchorsError, worm_store.WormError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return EXIT_FAIL
    except OSError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return EXIT_FAIL
    except KeyboardInterrupt:
        print("FAIL: interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
