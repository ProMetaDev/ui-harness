#!/usr/bin/env python3
"""本地 WORM 风格的内容寻址对象库（L2，可选；只用 Python 标准库）。

目标：把一份审计导出（``/api/audit/export`` 的 JSON）按**字节**存成不可变对象，
并在保留期到期之前拒绝删除。它模拟对象锁的**行为**，但不是 S3 Object Lock /
合规 WORM：没有服务端保留策略、没有可信时间戳、owner/root 仍可改文件。
边界见 ``docs/52-worm.md``。

布局::

    <store>/
      index.jsonl                   append-only 哈希链（0444）
      objects/<sha256>              对象字节（0444）
      objects/<sha256>.meta.json    sidecar 元数据（0444）
      tombstones/<sha256>.json      删除墓碑（0444）
      .lock                         单写者锁（flock；Windows 只用约定）

不变式：

- 对象 id = 对象字节的 SHA-256；对象路径由 id 决定，内容寻址；
- 同一个 id 只写一次：已存在时先校验哈希，绝不覆盖；重复 put 是幂等的；
- 保留期在 put 时固定，之后不能延长或缩短（重放同一个 put 带不同保留期会被拒绝）；
- 删除只在 ``now >= retentionUntil`` 时允许；删除后对象与其 sidecar 消失，
  留下 tombstone，且该 id 不能被重新 put（不允许“复活”）；
- ``index.jsonl`` 只追加：每条记录带 ``seq``、``prev`` 和 ``hash``（记录本身的
  SHA-256）。``verify`` 会重算整条链，并把它与磁盘上的对象/sidecar/tombstone
  互相核对。

崩溃窗口：delete 先追加 ``delete`` + ``tombstone`` 两条 index 记录，再写墓碑文件、
最后删对象。任何一步之间崩溃都会让 ``verify`` 报错；重跑同一个 ``delete`` 会补齐
缺失的墓碑文件、清掉残留对象。put 先把对象写进 ``objects/``，再写 sidecar，最后
追加 index；中途崩溃会留下“未入链”的文件，``verify`` 会报错，重跑 put 会补齐
sidecar 与 index（字节哈希已经验证过，所以不是覆盖）。

退出码：0 成功 / 1 输入或 I/O 错误 / 2 完整性或保留策略拒绝 / 64 用法错误。
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import re
import stat
import sys
import time
from pathlib import Path

ZERO = "0" * 64
OBJECTS_DIR = "objects"
TOMBSTONES_DIR = "tombstones"
INDEX_NAME = "index.jsonl"
LOCK_NAME = ".lock"
SIDECAR_SUFFIX = ".meta.json"
TOMBSTONE_SUFFIX = ".json"
FILE_MODE = 0o444
TMP_MODE = 0o600
# Windows 上 os.open 默认文本模式会把 \n 翻成 \r\n；对象必须逐字节不变。
BINARY = getattr(os, "O_BINARY", 0)

EXIT_OK = 0
EXIT_FAIL = 1
EXIT_REFUSED = 2
EXIT_USAGE = 64

ID_RE = re.compile(r"^[0-9a-f]{64}$")

FIELDS = {
    "put": ("seq", "op", "id", "prev", "ts", "retentionUntil", "size", "hash"),
    "delete": ("seq", "op", "id", "prev", "ts", "retentionUntil", "size", "hash"),
    "tombstone": (
        "seq",
        "op",
        "id",
        "prev",
        "ts",
        "deleteSeq",
        "retentionUntil",
        "size",
        "hash",
    ),
}
OPS = tuple(FIELDS)


class WormError(Exception):
    """普通错误（退出码 1）。"""

    def __init__(self, message: str, code: int = EXIT_FAIL) -> None:
        super().__init__(message)
        self.code = code


class Refused(WormError):
    """完整性或保留策略拒绝（退出码 2）。"""

    def __init__(self, message: str) -> None:
        super().__init__(message, EXIT_REFUSED)


class Parser(argparse.ArgumentParser):
    # argparse 默认的用法错误码是 2，会和“保留策略拒绝”的 2 混淆。
    def error(self, message: str) -> None:  # type: ignore[override]
        self.print_usage(sys.stderr)
        self.exit(EXIT_USAGE, f"{self.prog}: error: {message}\n")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def canonical(obj: dict) -> str:
    return json.dumps(obj, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def record_hash(record: dict) -> str:
    body = {k: v for k, v in record.items() if k != "hash"}
    return sha256_bytes(canonical(body).encode("utf-8"))


def record_line(record: dict) -> bytes:
    ordered = {k: record[k] for k in FIELDS[record["op"]]}
    return (json.dumps(ordered, ensure_ascii=False, separators=(",", ":")) + "\n").encode(
        "utf-8"
    )


def new_record(op: str, seq: int, prev: str, ts: int, oid: str, **extra: int) -> dict:
    record: dict = {"seq": seq, "op": op, "id": oid, "prev": prev, "ts": ts}
    record.update(extra)
    record["hash"] = record_hash(record)
    return record


def check_hex(value: object, name: str, where: str) -> str:
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(c not in "0123456789abcdef" for c in value)
    ):
        raise Refused(f"{where}: {name} must be 64 lowercase hex chars")
    return value


def check_nonneg_int(value: object, name: str, where: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise Refused(f"{where}: {name} must be a non-negative integer")
    return value


def normalize_id(value: str) -> str:
    oid = value.strip().lower()
    if not ID_RE.match(oid):
        raise WormError(
            f"object id must be 64 lowercase hex chars (got {value!r})", EXIT_USAGE
        )
    return oid


def normalize_now(value: int | None) -> int:
    if value is None:
        return int(time.time())
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise WormError("--now must be a non-negative integer (epoch seconds)", EXIT_USAGE)
    return value


def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        n = os.write(fd, view)
        if n <= 0:
            raise WormError("short write", EXIT_FAIL)
        view = view[n:]


def fsync_dir(path: Path) -> None:
    # Windows 不能以只读方式打开目录；失败就跳过（文件本身仍然 fsync）。
    try:
        fd = os.open(str(path), os.O_RDONLY)
    except OSError:
        return
    try:
        try:
            os.fsync(fd)
        except OSError:
            pass
    finally:
        os.close(fd)


def unlink_readonly(path: Path) -> bool:
    """删除文件；Windows 上先去掉只读属性，否则 unlink 会被拒绝。"""
    if not path.exists():
        return False
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
        if not mode & 0o200:
            os.chmod(path, mode | 0o200)
    except OSError:
        pass
    path.unlink()
    return True


def create_file(path: Path, data: bytes, mode: int = FILE_MODE) -> bool:
    """O_EXCL 新建文件；已存在返回 False（绝不覆盖）。"""
    try:
        fd = os.open(
            str(path), os.O_WRONLY | os.O_CREAT | os.O_EXCL | BINARY, TMP_MODE
        )
    except FileExistsError:
        return False
    try:
        try:
            write_all(fd, data)
            os.fsync(fd)
        finally:
            os.close(fd)
    except BaseException:
        # 写失败不要留下半截文件，否则下一次 put 会被哈希校验挡住。
        try:
            unlink_readonly(path)
        except OSError:
            pass
        raise
    os.chmod(path, mode)
    fsync_dir(path.parent)
    return True


def parse_index(raw: bytes) -> list[dict]:
    """解析并校验 index 链；任何结构问题都以 Refused 抛出。"""
    if not raw:
        return []
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise Refused(f"index is not valid UTF-8: {exc}") from exc
    if not text.endswith("\n"):
        raise Refused("index has a torn tail (last record does not end with a newline)")
    lines = text.split("\n")
    lines.pop()  # 末尾空串
    records: list[dict] = []
    prev_hash = ZERO
    for i, line in enumerate(lines, start=1):
        if not line.strip():
            raise Refused(f"index line {i}: blank lines are not allowed")
        try:
            record = json.loads(line)
        except json.JSONDecodeError as exc:
            raise Refused(f"index line {i}: invalid JSON: {exc}") from exc
        record = validate_record(record, i)
        if record["seq"] != i:
            raise Refused(f"index line {i}: seq {record['seq']} does not continue at {i}")
        if record["prev"] != prev_hash:
            raise Refused(
                f"index line {i}: prev {record['prev'][:12]}… does not match "
                f"previous hash {prev_hash[:12]}…"
            )
        records.append(record)
        prev_hash = record["hash"]
    return records


def validate_record(record: object, lineno: int) -> dict:
    where = f"index line {lineno}"
    if not isinstance(record, dict):
        raise Refused(f"{where}: record must be a JSON object")
    op = record.get("op")
    if op not in FIELDS:
        raise Refused(f"{where}: op must be one of {list(OPS)}, got {op!r}")
    expected = set(FIELDS[op])
    if set(record) != expected:
        raise Refused(
            f"{where}: expected exactly {sorted(expected)}, got {sorted(record)}"
        )
    check_nonneg_int(record["seq"], "seq", where)
    if record["seq"] < 1:
        raise Refused(f"{where}: seq must be >= 1")
    check_nonneg_int(record["ts"], "ts", where)
    check_nonneg_int(record["retentionUntil"], "retentionUntil", where)
    check_nonneg_int(record["size"], "size", where)
    check_hex(record["id"], "id", where)
    check_hex(record["prev"], "prev", where)
    check_hex(record["hash"], "hash", where)
    if op == "tombstone":
        check_nonneg_int(record["deleteSeq"], "deleteSeq", where)
        if record["deleteSeq"] < 1:
            raise Refused(f"{where}: deleteSeq must be >= 1")
    want = record_hash(record)
    if record["hash"] != want:
        raise Refused(
            f"{where}: record hash {record['hash'][:12]}… does not match content "
            f"{want[:12]}… (index was modified)"
        )
    return record


def scan_states(records: list[dict]) -> dict[str, dict]:
    """把记录折叠成每个 id 的状态；结构违规以 Refused 抛出。"""
    states: dict[str, dict] = {}
    for record in records:
        op = record["op"]
        oid = record["id"]
        state = states.setdefault(oid, {})
        where = f"index seq {record['seq']}"
        if op == "put":
            if "put" in state:
                raise Refused(f"{where}: duplicate put for {oid}")
            if "delete" in state or "tombstone" in state:
                raise Refused(f"{where}: put after delete/tombstone for {oid}")
            state["put"] = record
        elif op == "delete":
            if "put" not in state:
                raise Refused(f"{where}: delete without a prior put for {oid}")
            if "delete" in state:
                raise Refused(f"{where}: duplicate delete for {oid}")
            if "tombstone" in state:
                raise Refused(f"{where}: delete after tombstone for {oid}")
            put = state["put"]
            if record["retentionUntil"] != put["retentionUntil"]:
                raise Refused(f"{where}: delete retentionUntil does not match put")
            if record["size"] != put["size"]:
                raise Refused(f"{where}: delete size does not match put")
            if record["ts"] < put["ts"]:
                raise Refused(f"{where}: delete ts is before put ts")
            state["delete"] = record
        else:  # tombstone
            if "delete" not in state:
                raise Refused(f"{where}: tombstone without a prior delete for {oid}")
            if "tombstone" in state:
                raise Refused(f"{where}: duplicate tombstone for {oid}")
            delete = state["delete"]
            if record["deleteSeq"] != delete["seq"]:
                raise Refused(f"{where}: tombstone deleteSeq does not point at the delete")
            if record["retentionUntil"] != delete["retentionUntil"]:
                raise Refused(f"{where}: tombstone retentionUntil does not match delete")
            if record["size"] != delete["size"]:
                raise Refused(f"{where}: tombstone size does not match delete")
            if record["ts"] < delete["ts"]:
                raise Refused(f"{where}: tombstone ts is before delete ts")
            state["tombstone"] = record
    return states


def sidecar_bytes(oid: str, size: int, retention: int, created: int) -> bytes:
    payload = {
        "sha256": oid,
        "size": size,
        "retentionUntil": retention,
        "createdAt": created,
    }
    return (json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n").encode(
        "utf-8"
    )


def tombstone_bytes(record: dict) -> bytes:
    payload = {
        "deleteSeq": record["deleteSeq"],
        "deletedAt": record["ts"],
        "id": record["id"],
        "retentionUntil": record["retentionUntil"],
        "sha256": record["id"],
        "size": record["size"],
    }
    return (json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n").encode(
        "utf-8"
    )


@contextlib.contextmanager
def open_index_for_append(path: Path):
    """以 O_APPEND 打开 index；临时加 owner 写位，结束后尽量收回 0444。"""
    original = None
    if path.exists():
        original = stat.S_IMODE(path.stat().st_mode)
        if not original & 0o200:
            os.chmod(path, original | 0o200)
    try:
        fd = os.open(
            str(path), os.O_RDWR | os.O_APPEND | os.O_CREAT | BINARY, TMP_MODE
        )
    except OSError:
        if original is not None:
            try:
                os.chmod(path, original)
            except OSError:
                pass
        raise
    try:
        yield fd
    finally:
        try:
            if hasattr(os, "fchmod"):
                os.fchmod(fd, FILE_MODE)
            else:
                os.chmod(path, FILE_MODE)
        except OSError as exc:
            print(f"WARN: could not set mode 0444 on {path}: {exc}", file=sys.stderr)
        os.close(fd)


class WormStore:
    """一个本地对象库。所有变更都在 store 级排它锁下进行。"""

    def __init__(self, root: str | os.PathLike[str]) -> None:
        self.root = Path(root).expanduser()

    # ---- 路径 ----
    @property
    def index_path(self) -> Path:
        return self.root / INDEX_NAME

    @property
    def objects_dir(self) -> Path:
        return self.root / OBJECTS_DIR

    @property
    def tombstones_dir(self) -> Path:
        return self.root / TOMBSTONES_DIR

    def object_path(self, oid: str) -> Path:
        return self.objects_dir / oid

    def sidecar_path(self, oid: str) -> Path:
        return self.objects_dir / (oid + SIDECAR_SUFFIX)

    def tombstone_path(self, oid: str) -> Path:
        return self.tombstones_dir / (oid + TOMBSTONE_SUFFIX)

    # ---- 锁与布局 ----
    @contextlib.contextmanager
    def _lock(self, exclusive: bool):
        self.root.mkdir(parents=True, exist_ok=True)
        fd = os.open(
            str(self.root / LOCK_NAME), os.O_RDWR | os.O_CREAT | BINARY, TMP_MODE
        )
        fcntl = None
        try:
            import fcntl as _fcntl

            fcntl = _fcntl
        except ImportError:
            # Windows 没有 flock：单写者约定，见 docs/52。
            fcntl = None
        try:
            if fcntl is not None:
                fcntl.flock(fd, fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH)
            yield
        finally:
            if fcntl is not None:
                try:
                    fcntl.flock(fd, fcntl.LOCK_UN)
                except OSError:
                    pass
            os.close(fd)

    def _require_store(self) -> None:
        if not self.root.exists():
            raise WormError(f"store not found: {self.root}", EXIT_FAIL)
        if not self.root.is_dir():
            raise WormError(f"store is not a directory: {self.root}", EXIT_FAIL)

    def _ensure_layout(self) -> None:
        self.objects_dir.mkdir(parents=True, exist_ok=True)
        self.tombstones_dir.mkdir(parents=True, exist_ok=True)

    def _load(self) -> list[dict]:
        if not self.index_path.exists():
            return []
        with open(self.index_path, "rb") as f:
            return parse_index(f.read())

    def _append(self, records: list[dict]) -> None:
        if not records:
            return
        with open_index_for_append(self.index_path) as fd:
            for record in records:
                write_all(fd, record_line(record))
            os.fsync(fd)
        fsync_dir(self.root)

    # ---- sidecar / tombstone ----
    def _read_sidecar(self, oid: str) -> dict:
        path = self.sidecar_path(oid)
        if not path.is_file():
            raise Refused(f"sidecar missing for {oid}: {path}")
        try:
            meta = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise Refused(f"sidecar for {oid} is unreadable: {exc}") from exc
        expected = {"sha256", "size", "retentionUntil", "createdAt"}
        if not isinstance(meta, dict) or set(meta) != expected:
            raise Refused(f"sidecar for {oid} must have exactly {sorted(expected)}")
        if meta["sha256"] != oid:
            raise Refused(f"sidecar for {oid} names a different sha256")
        check_nonneg_int(meta["size"], "size", f"sidecar {oid}")
        check_nonneg_int(meta["retentionUntil"], "retentionUntil", f"sidecar {oid}")
        check_nonneg_int(meta["createdAt"], "createdAt", f"sidecar {oid}")
        return meta

    def _check_sidecar(self, meta: dict, oid: str, size: int, retention: int) -> None:
        if meta["sha256"] != oid:
            raise Refused(f"sidecar sha256 does not match {oid}")
        if meta["size"] != size:
            raise Refused(f"sidecar size does not match {size}")
        if meta["retentionUntil"] != retention:
            raise Refused(
                f"sidecar retentionUntil {meta['retentionUntil']} does not match "
                f"requested {retention}; retention is immutable"
            )

    def _check_object(self, oid: str, put: dict) -> None:
        path = self.object_path(oid)
        if not path.is_file():
            raise Refused(f"index has a put for {oid} but the object is missing")
        if path.stat().st_size != put["size"]:
            raise Refused(f"object {oid} size does not match the index")
        if sha256_file(path) != oid:
            raise Refused(f"object {oid} content hash does not match its id")
        meta = self._read_sidecar(oid)
        if (
            meta["sha256"] != oid
            or meta["size"] != put["size"]
            or meta["retentionUntil"] != put["retentionUntil"]
            or meta["createdAt"] != put["ts"]
        ):
            raise Refused(f"sidecar for {oid} does not match its put record")

    def _write_tombstone(self, record: dict) -> None:
        path = self.tombstone_path(record["id"])
        if path.exists():
            if self._read_tombstone(record["id"]) != json.loads(tombstone_bytes(record)):
                raise Refused(f"tombstone file for {record['id']} does not match the index")
            return
        _ = create_file(path, tombstone_bytes(record))

    def _read_tombstone(self, oid: str) -> dict:
        path = self.tombstone_path(oid)
        if not path.is_file():
            raise Refused(f"tombstone missing for {oid}: {path}")
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise Refused(f"tombstone for {oid} is unreadable: {exc}") from exc
        expected = {"deleteSeq", "deletedAt", "id", "retentionUntil", "sha256", "size"}
        if not isinstance(payload, dict) or set(payload) != expected:
            raise Refused(f"tombstone for {oid} must have exactly {sorted(expected)}")
        return payload

    def _remove_object_files(self, oid: str) -> None:
        unlink_readonly(self.object_path(oid))
        unlink_readonly(self.sidecar_path(oid))
        fsync_dir(self.objects_dir)

    def _finish_delete(
        self, records: list[dict], put: dict, delete: dict | None, now: int
    ) -> None:
        """把 delete 进行到底：必要时补 tombstone 记录、墓碑文件和清理。"""
        oid = put["id"]
        if delete is None:
            seq = len(records) + 1
            prev = records[-1]["hash"] if records else ZERO
            delete = new_record(
                "delete",
                seq,
                prev,
                now,
                oid,
                retentionUntil=put["retentionUntil"],
                size=put["size"],
            )
            tombstone = new_record(
                "tombstone",
                seq + 1,
                delete["hash"],
                delete["ts"],
                oid,
                deleteSeq=seq,
                retentionUntil=put["retentionUntil"],
                size=put["size"],
            )
            # 先入链，再落墓碑、再删文件；中间的崩溃由重跑 delete 修复。
            self._append([delete, tombstone])
        else:
            state_records = [r for r in records if r["id"] == oid]
            tombstone_rec = next(
                (r for r in state_records if r["op"] == "tombstone"), None
            )
            if tombstone_rec is None:
                seq = len(records) + 1
                prev = records[-1]["hash"] if records else ZERO
                tombstone = new_record(
                    "tombstone",
                    seq,
                    prev,
                    delete["ts"],
                    oid,
                    deleteSeq=delete["seq"],
                    retentionUntil=delete["retentionUntil"],
                    size=delete["size"],
                )
                self._append([tombstone])
            else:
                tombstone = tombstone_rec
        self._write_tombstone(tombstone)
        self._remove_object_files(oid)

    # ---- 公开 API ----
    def put(
        self,
        data: bytes,
        retention_until_epoch: int,
        now: int | None = None,
    ) -> str:
        """写入字节并返回 sha256 id。重复写入同一内容只做校验，不覆盖。"""
        if not isinstance(data, (bytes, bytearray)):
            raise WormError("put() expects bytes", EXIT_USAGE)
        data = bytes(data)
        retention = check_nonneg_int(
            retention_until_epoch, "retention_until_epoch", "put"
        )
        now = normalize_now(now)
        oid = sha256_bytes(data)

        with self._lock(exclusive=True):
            self._ensure_layout()
            records = self._load()
            states = scan_states(records)
            state = states.get(oid, {})
            if "tombstone" in state:
                raise Refused(
                    f"object {oid} is tombstoned; refusing to resurrect it "
                    "(a deleted retention id must not be reused)"
                )
            put = state.get("put")
            if put is not None:
                if put["retentionUntil"] != retention:
                    raise Refused(
                        f"object {oid} already exists with retentionUntil="
                        f"{put['retentionUntil']}; retention is immutable "
                        f"(requested {retention})"
                    )
                if put["size"] != len(data):
                    raise Refused(f"object {oid} already exists with a different size")
                self._check_object(oid, put)
                return oid

            # 没有 put 记录：要么是全新对象，要么是上次 put 中断留下的文件。
            obj = self.object_path(oid)
            if not create_file(obj, data):
                if not obj.is_file() or sha256_file(obj) != oid:
                    raise Refused(
                        f"object {oid} already exists with different bytes; "
                        "refusing to overwrite"
                    )
            sidecar = self.sidecar_path(oid)
            if sidecar.exists():
                meta = self._read_sidecar(oid)
                self._check_sidecar(meta, oid, len(data), retention)
                created = meta["createdAt"]
            else:
                created = now
                _ = create_file(sidecar, sidecar_bytes(oid, len(data), retention, created))
            seq = len(records) + 1
            prev = records[-1]["hash"] if records else ZERO
            record = new_record(
                "put",
                seq,
                prev,
                created,
                oid,
                retentionUntil=retention,
                size=len(data),
            )
            self._append([record])
        return oid

    def get(self, oid: str) -> bytes:
        oid = normalize_id(oid)
        with self._lock(exclusive=False):
            self._require_store()
            path = self.object_path(oid)
            if not path.is_file():
                raise WormError(f"object not found: {oid}", EXIT_FAIL)
            data = path.read_bytes()
        if sha256_bytes(data) != oid:
            raise Refused(f"object {oid} content hash does not match its id")
        return data

    def delete(self, oid: str, now: int | None = None) -> None:
        """保留期到期后删除对象并写 tombstone；未到期以 Refused 拒绝。"""
        oid = normalize_id(oid)
        now = normalize_now(now)
        with self._lock(exclusive=True):
            self._require_store()
            records = self._load()
            states = scan_states(records)
            state = states.get(oid)
            if state is None or "put" not in state:
                raise WormError(f"object not found: {oid}", EXIT_FAIL)
            put = state["put"]
            tombstone = state.get("tombstone")
            if tombstone is not None:
                # 已经删除过：只补齐可能被中断的文件操作，保持幂等。
                self._write_tombstone(tombstone)
                self._remove_object_files(oid)
                return
            delete = state.get("delete")
            if delete is None and now < put["retentionUntil"]:
                raise Refused(
                    f"retention until {put['retentionUntil']} has not expired "
                    f"(now={now}); refusing to delete {oid}"
                )
            self._finish_delete(records, put, delete, now)

    def list_objects(self, include_deleted: bool = False) -> list[dict]:
        with self._lock(exclusive=False):
            self._require_store()
            records = self._load()
            states = scan_states(records)
            rows: list[dict] = []
            for oid in sorted(states):
                state = states[oid]
                put = state.get("put")
                if put is None:
                    continue
                tombstone = state.get("tombstone")
                deleted = tombstone is not None
                if deleted and not include_deleted:
                    continue
                rows.append(
                    {
                        "id": oid,
                        "size": put["size"],
                        "createdAt": put["ts"],
                        "retentionUntil": put["retentionUntil"],
                        "deleted": deleted,
                        "deletedAt": tombstone["ts"] if deleted else None,
                    }
                )
            return rows

    def verify(self) -> dict:
        """检查对象哈希、sidecar、index 链与 tombstone；返回报告，不抛异常。"""
        report: dict = {
            "ok": False,
            "errors": [],
            "notes": [],
            "records": 0,
            "objects": 0,
            "tombstones": 0,
        }
        if not self.root.exists() or not self.root.is_dir():
            report["errors"].append(f"store not found: {self.root}")
            return report
        with self._lock(exclusive=False):
            try:
                records = self._load()
            except Refused as exc:
                report["errors"].append(str(exc))
                return report
            try:
                states = scan_states(records)
            except Refused as exc:
                report["errors"].append(str(exc))
                return report
            report["records"] = len(records)
            errors: list[str] = report["errors"]
            notes: list[str] = report["notes"]

            try:
                index_mode = stat.S_IMODE(self.index_path.stat().st_mode)
                report["indexMode"] = f"{index_mode:04o}"
                if index_mode & 0o222:
                    notes.append(
                        f"index.jsonl mode {index_mode:04o} is writable; "
                        "append-only is a convention, not WORM"
                    )
            except OSError:
                pass

            # 1) index -> 文件
            for oid in sorted(states):
                state = states[oid]
                put = state.get("put")
                if put is None:
                    errors.append(f"{oid}: delete/tombstone without put in index")
                    continue
                obj = self.object_path(oid)
                sidecar = self.sidecar_path(oid)
                tombstone = state.get("tombstone")
                if tombstone is None:
                    report["objects"] += 1
                    live_delete = state.get("delete")
                    if live_delete is not None:
                        errors.append(
                            f"{oid}: delete at seq {live_delete['seq']} has no "
                            "tombstone (interrupted delete; re-run delete)"
                        )
                    if not obj.is_file():
                        errors.append(f"{oid}: object file missing")
                        continue
                    try:
                        size = obj.stat().st_size
                    except OSError as exc:
                        errors.append(f"{oid}: cannot stat object: {exc}")
                        continue
                    if size != put["size"]:
                        errors.append(
                            f"{oid}: object size {size} != index size {put['size']}"
                        )
                    try:
                        digest = sha256_file(obj)
                    except OSError as exc:
                        errors.append(f"{oid}: cannot read object: {exc}")
                        continue
                    if digest != oid:
                        errors.append(
                            f"{oid}: object content hash {digest[:12]}… != id"
                        )
                    try:
                        meta = json.loads(sidecar.read_text(encoding="utf-8"))
                    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
                        errors.append(f"{oid}: sidecar missing or unreadable: {exc}")
                        meta = None
                    if meta is not None:
                        if not isinstance(meta, dict) or set(meta) != {
                            "sha256",
                            "size",
                            "retentionUntil",
                            "createdAt",
                        }:
                            errors.append(f"{oid}: sidecar fields are wrong")
                        elif (
                            meta["sha256"] != oid
                            or meta["size"] != put["size"]
                            or meta["retentionUntil"] != put["retentionUntil"]
                            or meta["createdAt"] != put["ts"]
                        ):
                            errors.append(f"{oid}: sidecar does not match index")
                    try:
                        mode = stat.S_IMODE(obj.stat().st_mode)
                        if mode & 0o222:
                            notes.append(
                                f"{oid}: object mode {mode:04o} is writable"
                            )
                    except OSError:
                        pass
                else:
                    report["tombstones"] += 1
                    if obj.exists():
                        errors.append(f"{oid}: tombstoned object file still exists")
                    if sidecar.exists():
                        errors.append(f"{oid}: tombstoned sidecar still exists")
                    try:
                        actual = self._read_tombstone(oid)
                    except Refused as exc:
                        errors.append(f"{oid}: {exc}")
                        continue
                    expected = json.loads(tombstone_bytes(tombstone))
                    if actual != expected:
                        errors.append(f"{oid}: tombstone file does not match index")

            # 2) 文件 -> index（未入链的文件、孤儿 sidecar/tombstone）
            try:
                entries = sorted(self.objects_dir.iterdir())
            except OSError:
                entries = []
            for entry in entries:
                name = entry.name
                if name.startswith("."):
                    continue  # 中断的临时文件；不参与链，但会在下面被点名
                if not entry.is_file():
                    errors.append(f"objects/{name}: unexpected non-file entry")
                    continue
                if name.endswith(SIDECAR_SUFFIX):
                    oid = name[: -len(SIDECAR_SUFFIX)]
                    if not ID_RE.match(oid):
                        errors.append(f"objects/{name}: unexpected sidecar name")
                    elif oid not in states or "put" not in states[oid]:
                        errors.append(f"objects/{name}: sidecar without a put record")
                    continue
                if not ID_RE.match(name):
                    errors.append(f"objects/{name}: unexpected file in objects/")
                    continue
                if name not in states or "put" not in states[name]:
                    errors.append(f"objects/{name}: unindexed object")
            try:
                tomb_entries = sorted(self.tombstones_dir.iterdir())
            except OSError:
                tomb_entries = []
            for entry in tomb_entries:
                name = entry.name
                if name.startswith("."):
                    continue
                if not entry.is_file() or not name.endswith(TOMBSTONE_SUFFIX):
                    errors.append(f"tombstones/{name}: unexpected entry")
                    continue
                oid = name[: -len(TOMBSTONE_SUFFIX)]
                if not ID_RE.match(oid):
                    errors.append(f"tombstones/{name}: unexpected name")
                elif oid not in states or "tombstone" not in states[oid]:
                    errors.append(f"tombstones/{name}: tombstone without an index record")

        report["ok"] = not report["errors"]
        return report


# ---------------------------------------------------------------- CLI


def read_input(path: str) -> bytes:
    if path == "-":
        return sys.stdin.buffer.read()
    return Path(path).expanduser().read_bytes()


def cmd_put(args: argparse.Namespace) -> int:
    now = normalize_now(args.now)
    if args.retention_until is not None:
        retention = args.retention_until
    else:
        days = args.retention_days
        if days is None or days < 0:
            raise WormError("--retention-days must be >= 0", EXIT_USAGE)
        retention = now + days * 86400
    if retention < 0:
        raise WormError("retention must be >= 0", EXIT_USAGE)
    data = read_input(args.file)
    oid = sha256_bytes(data)
    store = WormStore(args.store)
    existed = store.object_path(oid).is_file()
    store.put(data, retention, now=now)
    print(
        f"OK put id={oid} size={len(data)} retentionUntil={retention} "
        f"new={'no' if existed else 'yes'}"
    )
    return EXIT_OK


def cmd_get(args: argparse.Namespace) -> int:
    store = WormStore(args.store)
    data = store.get(args.id)
    if args.out:
        out = Path(args.out).expanduser()
        out.write_bytes(data)
        print(f"OK get id={args.id} bytes={len(data)} -> {out}")
    else:
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()
    return EXIT_OK


def cmd_delete(args: argparse.Namespace) -> int:
    store = WormStore(args.store)
    store.delete(args.id, now=args.now)
    oid = normalize_id(args.id)
    print(f"OK delete id={oid} tombstone={store.tombstone_path(oid)}")
    return EXIT_OK


def cmd_verify(args: argparse.Namespace) -> int:
    store = WormStore(args.store)
    report = store.verify()
    if args.json:
        print(json.dumps(report, ensure_ascii=False, sort_keys=True))
    elif report["ok"]:
        print(
            f"OK records={report['records']} objects={report['objects']} "
            f"tombstones={report['tombstones']}"
        )
        for note in report["notes"]:
            print(f"note: {note}")
    else:
        for problem in report["errors"]:
            print(f"FAIL: {problem}", file=sys.stderr)
    return EXIT_OK if report["ok"] else EXIT_REFUSED


def cmd_list(args: argparse.Namespace) -> int:
    store = WormStore(args.store)
    rows = store.list_objects(include_deleted=args.all)
    if args.json:
        print(json.dumps(rows, ensure_ascii=False, sort_keys=True))
        return EXIT_OK
    if not rows:
        print("(no objects)")
        return EXIT_OK
    print(f"{'id':64}  {'size':>10}  {'createdAt':>10}  {'retentionUntil':>14}  state")
    for row in rows:
        state = "deleted" if row["deleted"] else "live"
        print(
            f"{row['id']}  {row['size']:>10}  {row['createdAt']:>10}  "
            f"{row['retentionUntil']:>14}  {state}"
        )
    return EXIT_OK


def build_parser() -> Parser:
    p = Parser(
        prog="store.py",
        description=(
            "Local WORM-style content-addressed store for /api/audit/export "
            "(L2, optional; NOT S3 Object Lock)"
        ),
    )
    sub = p.add_subparsers(dest="command", required=True)

    put = sub.add_parser("put", help="store bytes and print the sha256 object id")
    put.add_argument("--store", required=True, help="store root directory")
    put.add_argument("--file", required=True, help="input file, or '-' for stdin")
    group = put.add_mutually_exclusive_group(required=True)
    group.add_argument(
        "--retention-until", type=int, help="retention end (epoch seconds)"
    )
    group.add_argument(
        "--retention-days", type=int, help="retention end = now + N days"
    )
    put.add_argument(
        "--now", type=int, help="override the local clock (epoch seconds; tests only)"
    )
    put.set_defaults(func=cmd_put)

    get = sub.add_parser("get", help="write object bytes to stdout or a file")
    get.add_argument("--store", required=True)
    get.add_argument("id")
    get.add_argument("--out", help="output file (default: stdout)")
    get.set_defaults(func=cmd_get)

    delete = sub.add_parser("delete", help="delete after retention and write a tombstone")
    delete.add_argument("--store", required=True)
    delete.add_argument("id")
    delete.add_argument(
        "--now", type=int, help="override the local clock (epoch seconds; tests only)"
    )
    delete.set_defaults(func=cmd_delete)

    verify = sub.add_parser("verify", help="verify hashes, sidecars, index chain, tombstones")
    verify.add_argument("--store", required=True)
    verify.add_argument("--json", action="store_true")
    verify.set_defaults(func=cmd_verify)

    listing = sub.add_parser("list", help="list live objects (or all with --all)")
    listing.add_argument("--store", required=True)
    listing.add_argument("--all", action="store_true", help="include deleted objects")
    listing.add_argument("--json", action="store_true")
    listing.set_defaults(func=cmd_list)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except WormError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return exc.code
    except OSError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return EXIT_FAIL


if __name__ == "__main__":
    sys.exit(main())
