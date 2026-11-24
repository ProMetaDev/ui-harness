#!/usr/bin/env python3
"""审计导出哈希的 append-only 锚定链（L2，可选；只用 Python 标准库）。

``anchors.jsonl`` 每行一条记录，字段固定为：

    {"seq":1,"sha256":"<导出文件的 sha256>","prev":"<上一条记录的哈希>","ts":<毫秒>}

规则：
- ``seq`` 从 1 开始且连续；
- ``sha256`` 是导出文件**字节**的 SHA-256；
- ``prev`` 是上一条记录的哈希；第一条用 64 个 ``0``（创世）；
- 记录哈希 = SHA-256(canonical JSON：对 seq/sha256/prev/ts 四字段按键排序、
  无空格序列化)。因此改动 ``ts`` 或 ``seq`` 也会被下一条的 ``prev`` 发现。

边界：这是文件权限 + 追加写约定，不是 WORM、不是对象锁、不是可信时间戳。
没有外部记录的 head 时，删掉尾部若干条仍是一条自洽的链；用 ``--expect-head``
或 ``--expect-count`` 才能把截断钉到某个已知锚点。

退出码：0 正常 / 2 链断裂或被改 / 3 截断或尾部撕裂 / 64 用法错误。
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import stat
import sys
import time
from pathlib import Path

ZERO = "0" * 64
FIELDS = ("seq", "sha256", "prev", "ts")

EXIT_OK = 0
EXIT_BAD = 2
EXIT_TRUNC = 3
EXIT_USAGE = 64


class AnchorsError(Exception):
    def __init__(self, message: str, code: int = EXIT_BAD) -> None:
        super().__init__(message)
        self.code = code


class Parser(argparse.ArgumentParser):
    # argparse 默认的用法错误码是 2，会和“链断裂”的 2 混淆。
    def error(self, message: str) -> None:  # type: ignore[override]
        self.print_usage(sys.stderr)
        self.exit(EXIT_USAGE, f"{self.prog}: error: {message}\n")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def check_hex(value: object, name: str, where: str) -> str:
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(c not in "0123456789abcdef" for c in value)
    ):
        raise AnchorsError(f"{where}: {name} must be 64 lowercase hex chars")
    return value


def canonical(entry: dict) -> str:
    return json.dumps(
        {k: entry[k] for k in FIELDS},
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    )


def entry_hash(entry: dict) -> str:
    return hashlib.sha256(canonical(entry).encode("utf-8")).hexdigest()


def line_bytes(entry: dict) -> bytes:
    # 人类可读的字段顺序：seq, sha256, prev, ts。
    ordered = {k: entry[k] for k in FIELDS}
    return (
        json.dumps(ordered, ensure_ascii=False, separators=(",", ":")) + "\n"
    ).encode("utf-8")


def validate_entry(entry: object, lineno: int) -> dict:
    where = f"line {lineno}"
    if not isinstance(entry, dict):
        raise AnchorsError(f"{where}: record must be a JSON object")
    if set(entry) != set(FIELDS):
        raise AnchorsError(
            f"{where}: expected exactly {list(FIELDS)}, got {sorted(entry)}"
        )
    seq = entry["seq"]
    if not isinstance(seq, int) or isinstance(seq, bool) or seq < 1:
        raise AnchorsError(f"{where}: seq must be a positive integer")
    ts = entry["ts"]
    if not isinstance(ts, int) or isinstance(ts, bool) or ts < 0:
        raise AnchorsError(f"{where}: ts must be a non-negative integer (ms)")
    check_hex(entry["sha256"], "sha256", where)
    prev = entry["prev"]
    if lineno == 1:
        if prev != ZERO:
            raise AnchorsError(
                f"{where}: prev must be the genesis hash {ZERO[:12]}…, got {prev!r}"
            )
    else:
        check_hex(prev, "prev", where)
    return entry


def parse_raw(raw: bytes) -> tuple[list[dict], bool]:
    """返回 (entries, torn_tail)。torn_tail=True 表示最后一行不完整。"""
    if not raw:
        return [], False
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise AnchorsError(f"anchors file is not valid UTF-8: {exc}") from exc
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    entries: list[dict] = []
    torn = False
    for i, line in enumerate(lines, start=1):
        if not line.strip():
            raise AnchorsError(f"line {i}: blank lines are not allowed")
        try:
            obj = json.loads(line)
        except json.JSONDecodeError as exc:
            if i == len(lines):
                # 尾部撕裂（例如写入过程中崩溃）：前缀仍可校验。
                torn = True
                break
            raise AnchorsError(f"line {i}: invalid JSON: {exc}") from exc
        entries.append(validate_entry(obj, i))
    return entries, torn


def read_entries(path: Path) -> tuple[list[dict], bool]:
    if not path.exists():
        return [], False
    with open(path, "rb") as f:
        return parse_raw(f.read())


def check_chain(entries: list[dict]) -> list[str]:
    hashes: list[str] = []
    prev = ZERO
    for i, entry in enumerate(entries, start=1):
        validate_entry(entry, i)
        if entry["seq"] != i:
            raise AnchorsError(
                f"line {i}: seq {entry['seq']} does not continue at {i}"
            )
        if entry["prev"] != prev:
            raise AnchorsError(
                f"line {i}: prev {entry['prev'][:12]}… does not match "
                f"previous hash {prev[:12]}…"
            )
        h = entry_hash(entry)
        hashes.append(h)
        prev = h
    return hashes


def read_from_fd(fd: int) -> tuple[list[dict], bool]:
    os.lseek(fd, 0, os.SEEK_SET)
    chunks: list[bytes] = []
    while True:
        chunk = os.read(fd, 65536)
        if not chunk:
            break
        chunks.append(chunk)
    return parse_raw(b"".join(chunks))


@contextlib.contextmanager
def open_for_append(path: Path, mode: int = 0o444):
    """打开 anchors 文件并加排它锁；退出时把权限设回 0444（尽力而为）。"""
    original = None
    if path.exists():
        original = stat.S_IMODE(path.stat().st_mode)
        if not original & 0o200:
            # 0444 下无法以写方式打开；临时加 owner 写位，结束后再收回。
            os.chmod(path, original | 0o200)
    try:
        fd = os.open(path, os.O_RDWR | os.O_APPEND | os.O_CREAT, 0o600)
    except OSError:
        # 打不开就把临时加的写位还回去，不把 0444 留成 0644。
        if original is not None:
            try:
                os.chmod(path, original)
            except OSError:
                pass
        raise
    try:
        try:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_EX)
        except ImportError:  # Windows：单写者约定，靠文档而不是 flock。
            pass
        yield fd
    finally:
        try:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_UN)
        except ImportError:
            pass
        try:
            if hasattr(os, "fchmod"):
                os.fchmod(fd, mode)
            else:
                os.chmod(path, mode)
        except OSError as exc:
            print(f"WARN: could not set mode {mode:04o} on {path}: {exc}", file=sys.stderr)
        os.close(fd)


def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        n = os.write(fd, view)
        if n <= 0:
            raise AnchorsError("short write while appending anchor", EXIT_BAD)
        view = view[n:]


def cmd_append(args: argparse.Namespace) -> int:
    anchors = Path(args.anchors).expanduser()
    if bool(args.export) == bool(args.sha256):
        raise AnchorsError(
            "give exactly one of --export (hash the file) or --sha256 (known hash)",
            EXIT_USAGE,
        )
    if args.export:
        export = Path(args.export).expanduser()
        if not export.is_file():
            raise AnchorsError(f"export not found: {export}", EXIT_USAGE)
        digest = sha256_file(export)
    else:
        digest = args.sha256.lower()
        check_hex(digest, "sha256", "--sha256")
    ts = int(time.time() * 1000) if args.ts is None else args.ts
    if ts < 0:
        raise AnchorsError("--ts must be a non-negative millisecond timestamp", EXIT_USAGE)

    with open_for_append(anchors) as fd:
        entries, torn = read_from_fd(fd)
        if torn:
            raise AnchorsError(
                "anchors file has a torn tail; refusing to append to a broken chain",
                EXIT_TRUNC,
            )
        hashes = check_chain(entries)
        seq = entries[-1]["seq"] + 1 if entries else 1
        prev = hashes[-1] if hashes else ZERO
        entry = {"seq": seq, "sha256": digest, "prev": prev, "ts": ts}
        write_all(fd, line_bytes(entry))
        os.fsync(fd)
        head = entry_hash(entry)

    print(
        f"OK appended seq={seq} sha256={digest} prev={prev} "
        f"ts={ts} head={head}"
    )
    return EXIT_OK


def parse_expect_head(value: str | None) -> str | None:
    if value is None:
        return None
    value = value.lower()
    if len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
        raise AnchorsError("--expect-head must be 64 lowercase hex chars", EXIT_USAGE)
    return value


def cmd_verify(args: argparse.Namespace) -> int:
    anchors = Path(args.anchors).expanduser()
    if not anchors.is_file():
        raise AnchorsError(f"anchors file not found: {anchors}")
    entries, torn = read_entries(anchors)
    if torn:
        print(
            f"FAIL: {anchors} has a torn tail (last line is incomplete); "
            "the readable prefix was not trusted",
            file=sys.stderr,
        )
        return EXIT_TRUNC
    hashes = check_chain(entries)
    count = len(entries)
    head = hashes[-1] if hashes else ZERO
    last = entries[-1] if entries else None

    expected_head = parse_expect_head(args.expect_head)
    checkpoint_note = ""
    if args.expect_count is not None:
        if args.expect_count < 0:
            raise AnchorsError("--expect-count must be >= 0", EXIT_USAGE)
        if count < args.expect_count:
            print(
                f"FAIL truncated: {count} anchors < expected {args.expect_count} "
                f"(head={head})",
                file=sys.stderr,
            )
            return EXIT_TRUNC
    if expected_head is not None:
        if not hashes:
            # 空链的 head 是创世哈希；只有期望值也是创世时才算“还没开始”。
            if expected_head != ZERO:
                print(
                    f"FAIL truncated: empty anchors file, expected head {expected_head}",
                    file=sys.stderr,
                )
                return EXIT_TRUNC
        elif expected_head != ZERO and expected_head not in hashes:
            print(
                f"FAIL truncated or rewritten: checkpoint head {expected_head} "
                f"not found in {count} anchors (current head={head})",
                file=sys.stderr,
            )
            return EXIT_TRUNC
        elif expected_head == ZERO:
            # 创世 checkpoint 总是“存在”：之后追加只是正常增长。
            checkpoint_note = (
                f"genesis checkpoint is trivially present; "
                f"chain extends to head={head} (seq={last['seq']})"
            )
        elif expected_head != head:
            # 找到 checkpoint 后又追加了记录：这是正常增长，不是截断。
            checkpoint_note = (
                f"checkpoint {expected_head[:12]}… is present; "
                f"chain extends to head={head} (seq={last['seq']})"
            )

    try:
        mode = stat.S_IMODE(anchors.stat().st_mode)
    except OSError:
        mode = None
    note = ""
    if mode is not None and mode & 0o222:
        note = (
            f" (note: mode {mode:04o} is writable; append-only is a convention, not WORM)"
        )
    payload = {
        "ok": True,
        "count": count,
        "head": head,
        "sha256": last["sha256"] if last else "",
        "seq": last["seq"] if last else 0,
        "ts": last["ts"] if last else 0,
        "mode": f"{mode:04o}" if mode is not None else None,
        "checkpoint_found": expected_head is not None,
    }
    if args.json:
        print(json.dumps(payload, ensure_ascii=False, sort_keys=True))
    else:
        message = (
            f"OK {count} anchors, head={head}, "
            f"last_sha256={payload['sha256'] or '-'}"
        )
        if checkpoint_note:
            message += f"; {checkpoint_note}"
        print(message + note)
    return EXIT_OK


def cmd_head(args: argparse.Namespace) -> int:
    anchors = Path(args.anchors).expanduser()
    if not anchors.is_file():
        raise AnchorsError(f"anchors file not found: {anchors}")
    entries, torn = read_entries(anchors)
    if torn:
        print("FAIL: torn tail; refusing to report a head", file=sys.stderr)
        return EXIT_TRUNC
    hashes = check_chain(entries)
    last = entries[-1] if entries else None
    payload = {
        "count": len(entries),
        "seq": last["seq"] if last else 0,
        "head": hashes[-1] if hashes else ZERO,
        "sha256": last["sha256"] if last else "",
        "prev": last["prev"] if last else "",
        "ts": last["ts"] if last else 0,
    }
    if args.print:
        print(payload[args.print])
    elif args.json:
        print(json.dumps(payload, ensure_ascii=False, sort_keys=True))
    else:
        print(
            f"count={payload['count']} seq={payload['seq']} "
            f"head={payload['head']} sha256={payload['sha256'] or '-'}"
        )
    return EXIT_OK


def build_parser() -> Parser:
    p = Parser(
        prog="anchor.py",
        description="append-only hash anchors for /api/audit/export (L2, optional)",
    )
    sub = p.add_subparsers(dest="command", required=True)

    a = sub.add_parser("append", help="append the hash of an export file")
    a.add_argument("--anchors", default="anchors.jsonl")
    a.add_argument("--export", help="export file to hash")
    a.add_argument("--sha256", help="already-computed export hash (hex)")
    a.add_argument("--ts", type=int, help="timestamp in ms (default: now)")
    a.set_defaults(func=cmd_append)

    v = sub.add_parser("verify", help="verify the whole chain")
    v.add_argument("--anchors", default="anchors.jsonl")
    v.add_argument(
        "--expect-head",
        help="previously recorded head hash; if absent the chain is truncated",
    )
    v.add_argument(
        "--expect-count",
        type=int,
        help="previously recorded anchor count; fewer entries means truncation",
    )
    v.add_argument("--json", action="store_true")
    v.set_defaults(func=cmd_verify)

    h = sub.add_parser("head", help="print the latest anchor head")
    h.add_argument("--anchors", default="anchors.jsonl")
    h.add_argument(
        "--print",
        choices=("head", "sha256", "prev", "seq", "count", "ts"),
        help="print one bare value for scripting",
    )
    h.add_argument("--json", action="store_true")
    h.set_defaults(func=cmd_head)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except AnchorsError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        if exc.code == EXIT_USAGE:
            return EXIT_USAGE
        return exc.code
    except OSError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return EXIT_BAD


if __name__ == "__main__":
    sys.exit(main())
