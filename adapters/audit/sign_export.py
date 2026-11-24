#!/usr/bin/env python3
"""审计导出 JSON 的 OpenSSH 非对称签名/验签（L2，可选）。

约束：
- 只用 Python 标准库 + 系统 ``ssh-keygen``（OpenSSH >= 8.1 的 -Y sign/verify）；
- 不修改导出 JSON：对逐字节副本签名，签名只覆盖文件内容；
- 不打印私钥内容，命令输出里只有路径与哈希。

用法：
    python3 adapters/audit/sign_export.py sign \
        --export export.json --key ~/.ssh/audit_ed25519
    python3 adapters/audit/sign_export.py verify \
        --export export.json --sig export.json.sig \
        --allowed-signers allowed_signers --identity auditor@example.com

退出码：0 成功 / 1 验签失败或输入错误 / 2 SKIP（没有 ssh-keygen 或太老）
        / 64 用法错误。
"""
from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

DEFAULT_NAMESPACE = "ui-audit"
EXIT_OK = 0
EXIT_FAIL = 1
EXIT_SKIP = 2
EXIT_USAGE = 64

_NAMESPACE_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._@-]*$")


class Skip(Exception):
    """当前环境没有可用的 OpenSSH 签名能力（退出码 2）。"""


class Fail(Exception):
    """验签失败或输入不合法（退出码 1）。"""


class Parser(argparse.ArgumentParser):
    # argparse 默认用 2 表示用法错误，会和 SKIP 的 2 混淆。
    def error(self, message: str) -> None:  # type: ignore[override]
        self.print_usage(sys.stderr)
        self.exit(EXIT_USAGE, f"{self.prog}: error: {message}\n")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def find_ssh_keygen() -> str:
    exe = shutil.which("ssh-keygen")
    if not exe:
        raise Skip("ssh-keygen not found in PATH (OpenSSH >= 8.1 is required)")
    return exe


def require_sign_support(exe: str) -> None:
    p = subprocess.run(
        [exe, "-Y", "sign"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        errors="replace",
    )
    blob = (p.stdout + "\n" + p.stderr).lower()
    if any(m in blob for m in ("unknown option", "illegal option", "invalid option")):
        raise Skip(
            f"{exe} does not support 'ssh-keygen -Y sign' (OpenSSH >= 8.1 is required)"
        )


def check_namespace(namespace: str) -> None:
    if not _NAMESPACE_RE.match(namespace):
        raise Fail(
            "namespace must match [A-Za-z0-9][A-Za-z0-9._@-]* "
            f"(got {namespace!r})"
        )


def tail(text: str, limit: int = 500) -> str:
    text = (text or "").strip()
    if not text:
        return "(no output)"
    return text[-limit:]


def warn_loose_key_permissions(key: Path) -> None:
    # 只提示，不读取、不打印私钥内容。
    if os.name != "posix":
        return
    try:
        mode = stat.S_IMODE(key.stat().st_mode)
    except OSError:
        return
    if mode & 0o077:
        print(
            f"WARN: private key {key} is accessible by group/other "
            f"(mode {mode:04o}); chmod 600 is recommended",
            file=sys.stderr,
        )


def atomic_write(path: Path, data: bytes, mode: int = 0o644) -> None:
    path = path.expanduser()
    fd, tmp_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=str(path.parent)
    )
    tmp = Path(tmp_name)
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        try:
            os.chmod(tmp, mode)
        except OSError:
            pass
        os.replace(tmp, path)
    finally:
        if tmp.exists():
            try:
                tmp.unlink()
            except OSError:
                pass


def run_verify(
    exe: str,
    export: Path,
    sig: Path,
    allowed_signers: Path,
    identity: str,
    namespace: str,
) -> None:
    if not identity or identity.startswith("-"):
        raise Fail("identity must be non-empty and must not start with '-'")
    if not allowed_signers.is_file():
        raise Fail(f"allowed_signers not found: {allowed_signers}")
    if not sig.is_file():
        raise Fail(f"signature not found: {sig}")
    with open(export, "rb") as payload:
        p = subprocess.run(
            [
                exe,
                "-Y",
                "verify",
                "-f",
                str(allowed_signers),
                "-I",
                identity,
                "-n",
                namespace,
                "-s",
                str(sig),
            ],
            stdin=payload,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
        )
    if p.returncode != 0:
        detail = tail(p.stderr) if p.stderr.strip() else tail(p.stdout)
        raise Fail(
            f"signature verification failed for {export} "
            f"(identity={identity}, namespace={namespace}): {detail}"
        )


def cmd_sign(args: argparse.Namespace) -> int:
    export = Path(args.export).expanduser()
    key = Path(args.key).expanduser()
    if not export.is_file():
        raise Fail(f"export not found: {export}")
    if not key.is_file():
        raise Fail(f"key not found: {key}")
    check_namespace(args.namespace)
    if bool(args.allowed_signers) != bool(args.identity):
        raise Fail(
            "--allowed-signers and --identity must be given together "
            "when a self-check after signing is requested"
        )

    exe = find_ssh_keygen()
    require_sign_support(exe)
    warn_loose_key_permissions(key)

    sig_path = Path(args.sig).expanduser() if args.sig else Path(str(export) + ".sig")
    try:
        if sig_path.resolve() == export.resolve():
            raise Fail("--sig must not point at the export file itself")
    except OSError:
        pass
    if sig_path.exists() and not args.force:
        raise Fail(f"signature already exists: {sig_path} (use --force to overwrite)")

    before = sha256_file(export)
    # 对逐字节副本签名：ssh-keygen 只会写副本旁边的 .sig，
    # 原导出文件连 mtime 都不会被碰。
    with tempfile.TemporaryDirectory(prefix="ui-audit-sign.") as td:
        payload = Path(td) / "export.json"
        shutil.copyfile(export, payload)
        p = subprocess.run(
            [
                exe,
                "-Y",
                "sign",
                "-f",
                str(key),
                "-n",
                args.namespace,
                str(payload),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
        )
        if p.returncode != 0:
            raise Fail(
                f"ssh-keygen sign failed (exit {p.returncode}): {tail(p.stderr)}"
            )
        # 防御性检查：签名前后导出文件必须逐字节一致。
        if sha256_file(export) != before:
            raise Fail("export changed while signing; refusing to keep the signature")
        produced = Path(str(payload) + ".sig")
        if not produced.is_file():
            raise Fail("ssh-keygen reported success but produced no .sig file")
        sig_data = produced.read_bytes()

    atomic_write(sig_path, sig_data)
    print(
        f"OK signed {export} -> {sig_path} "
        f"(namespace={args.namespace}, sha256={before})"
    )
    if args.allowed_signers:
        run_verify(
            exe,
            export,
            sig_path,
            Path(args.allowed_signers).expanduser(),
            args.identity,
            args.namespace,
        )
        print("OK signature self-check passed")
    return EXIT_OK


def cmd_verify(args: argparse.Namespace) -> int:
    export = Path(args.export).expanduser()
    sig = Path(args.sig).expanduser() if args.sig else Path(str(export) + ".sig")
    allowed = Path(args.allowed_signers).expanduser()
    if not export.is_file():
        raise Fail(f"export not found: {export}")
    if not args.identity:
        raise Fail("--identity is required")
    check_namespace(args.namespace)

    exe = find_ssh_keygen()
    require_sign_support(exe)
    run_verify(exe, export, sig, allowed, args.identity, args.namespace)
    print(
        f"OK verified {export} with {sig} "
        f"(identity={args.identity}, namespace={args.namespace})"
    )
    return EXIT_OK


def cmd_check(_args: argparse.Namespace) -> int:
    exe = find_ssh_keygen()
    require_sign_support(exe)
    print(f"OK ssh-keygen={exe} supports -Y sign/verify")
    return EXIT_OK


def build_parser() -> Parser:
    p = Parser(
        prog="sign_export.py",
        description="OpenSSH detached signature for /api/audit/export (L2, optional)",
    )
    sub = p.add_subparsers(dest="command", required=True)

    s = sub.add_parser("sign", help="sign an export JSON, producing a detached .sig")
    s.add_argument("--export", required=True, help="path to the export JSON")
    s.add_argument("--key", required=True, help="OpenSSH private key used to sign")
    s.add_argument("--sig", help="signature output (default: <export>.sig)")
    s.add_argument("--namespace", default=DEFAULT_NAMESPACE)
    s.add_argument(
        "--identity",
        help="optional: verify immediately after signing (requires --allowed-signers)",
    )
    s.add_argument(
        "--allowed-signers",
        help="optional: verify immediately after signing (requires --identity)",
    )
    s.add_argument("--force", action="store_true", help="overwrite an existing .sig")
    s.set_defaults(func=cmd_sign)

    v = sub.add_parser("verify", help="verify a detached .sig against an export JSON")
    v.add_argument("--export", required=True, help="path to the export JSON")
    v.add_argument("--sig", help="signature path (default: <export>.sig)")
    v.add_argument("--allowed-signers", required=True)
    v.add_argument("--identity", required=True)
    v.add_argument("--namespace", default=DEFAULT_NAMESPACE)
    v.set_defaults(func=cmd_verify)

    c = sub.add_parser("check", help="check whether ssh-keygen supports -Y sign/verify")
    c.set_defaults(func=cmd_check)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except Skip as exc:
        print(f"SKIP: {exc}", file=sys.stderr)
        return EXIT_SKIP
    except Fail as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return EXIT_FAIL
    except OSError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return EXIT_FAIL


if __name__ == "__main__":
    sys.exit(main())
