#!/usr/bin/env python3
"""最小 Postgres worker：claim -> run ui -> heartbeat -> done/failed/cancelled。

只使用 Python 标准库 + 可选 psql（L2 adapter）。不进入 ui-core。

三件事必须 fail closed，不能"看起来成功"：

1. 会话/工作区映射：claim 回来的 session 与 workspace_id 必须能映射到一个
   安全的工作区目录，并且该目录在 workspace root 之内。映射缺失/越界时任务
   直接写 failed 并记 last_error，绝不回退到 self.workspace 里跑。
2. lease：heartbeat 一旦发现自己不再持有任务/工作区租约（或连不上数据库），
   主循环立刻终止本地 `ui` 子进程，写 failed，绝不写 done。
3. 协议：claim 用数据库端 json_build_object/json_agg 输出 JSON；事件同步把
   SQL 从 stdin 递给 psql，不把大 JSON 拼进命令行。

取消语义（原有行为保持）：
- claim 前先把自己名下 running 且 cancel_requested 的任务标成 cancelled；
- claim 到 cancel_requested=true 的任务时不启动 ui；
- 运行中由 heartbeat 轮询 cancel_requested，命中后终止子进程并标 cancelled；
- 任何路径都不把已请求取消的任务写成 done。
"""
import argparse
import datetime as dt
import json
import os
import re
import subprocess
import sys
import threading
import time

# Heartbeat also carries the cancellation poll; keep it well below the 60s lease.
HEARTBEAT_SECONDS = 2.0
UI_POLL_SECONDS = 0.2
# Postgres 端的 lease 是 60s；Python 侧只做文档说明，不参与续租决策。
ERROR_MAX = 1000

# 数据库端负责 JSON 序列化：worker 只 json.loads，不再按 tab 切分。
# prompt 里可能有 tab / 换行 / 引号，按分隔符切一定会错位。
CLAIM_JSON = "select ui_claim_task_json('{worker}')"

# `ui run` 的这些选项由 worker 从 claim 结果拼出来，extra 里不许再出现，
# 否则重复选项会覆盖映射好的 --workspace/--session（CLI 取最后一个值）。
RESERVED_UI_OPTIONS = ("--workspace", "--session", "--journal", "--task", "--task-file")

DRIVE_RE = re.compile(r"^[A-Za-z]:")


class WorkspaceError(RuntimeError):
    """工作区映射缺失/越界。调用方必须 fail closed。"""


class SessionError(RuntimeError):
    """会话名不安全，交给 `ui run --session` 会静默退回 default。"""


def q(sql):
    return sql.replace("'", "''")


def lit(value):
    if value is None:
        return "NULL"
    return "'" + str(value).replace("'", "''") + "'"


def valid_session(name):
    """与 ui-cli `valid_session_name` 同一条规则。

    ui 的规则是：空 / 超长 / 非 [A-Za-z0-9_-] 的名字会静默退回 default。
    对 worker 来说那是错的目录和错的 journal，所以这里必须先拒绝。
    """
    return bool(
        isinstance(name, str)
        and 0 < len(name) <= 64
        and all(c.isascii() and (c.isalnum() or c in "-_") for c in name)
    )


def _safe_parts(rel):
    """把映射值拆成安全的相对路径分量。

    拒绝绝对路径（含 Windows 盘符/UNC）、`.`/`..`、隐藏目录分量、NUL 和
    空分量。返回值一定是相对路径分量列表。
    """
    if not isinstance(rel, str) or not rel:
        raise WorkspaceError("workspace mapping must be a non-empty string")
    if "\x00" in rel:
        raise WorkspaceError("workspace mapping contains NUL")
    if DRIVE_RE.match(rel) or rel.startswith(("/", "\\")):
        raise WorkspaceError(f"workspace mapping must be relative: {rel!r}")
    parts = [p for p in re.split(r"[\\/]+", rel) if p != ""]
    if not parts:
        raise WorkspaceError(f"workspace mapping is empty: {rel!r}")
    for part in parts:
        if part in (".", ".."):
            raise WorkspaceError(
                f"workspace mapping may not contain '.' or '..': {rel!r}"
            )
        if part.startswith("."):
            raise WorkspaceError(
                f"workspace mapping may not use hidden components: {rel!r}"
            )
        if part != part.strip():
            raise WorkspaceError(
                f"workspace mapping component has surrounding whitespace: {rel!r}"
            )
    return parts


def load_registry(path):
    """读取 `--workspace-registry` 的 JSON：{workspace_id: 相对路径}。

    这是"明确映射"：只有登记过的 workspace_id 才会用登记路径；相对路径仍然
    要在 root 之内，绝对路径/`..` 在启动时就被拒绝（不是运行到一半才发现）。
    """
    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
    except OSError as exc:
        raise SystemExit(f"无法读取 workspace registry {path}: {exc}")
    except json.JSONDecodeError as exc:
        raise SystemExit(f"workspace registry {path} 不是合法 JSON: {exc}")
    if not isinstance(data, dict):
        raise SystemExit(f"workspace registry {path} 必须是 {{workspace_id: 相对路径}} 对象")
    registry = {}
    for key, value in data.items():
        if not isinstance(key, str) or not key:
            raise SystemExit(f"workspace registry 的 key 必须是非空字符串: {key!r}")
        try:
            parts = _safe_parts(value)
        except WorkspaceError as exc:
            raise SystemExit(f"workspace registry[{key!r}] 不合法: {exc}")
        registry[key] = os.path.join(*parts)
    return registry


class WorkspaceResolver:
    """workspace_id -> 安全的本地目录。

    两种模式：

    - 单工作区（兼容旧用法）：`--workspace DIR [--workspace-id ID]`。只有
      ID 一个映射；别的 workspace_id 一律 fail closed（绝不"就近"跑在 DIR 里）。
    - 多工作区：`--workspace-root ROOT`。默认映射 ROOT/<workspace_id>（要求
      workspace_id 是单个安全分量），`--workspace-registry` 可以覆盖成
      ROOT/<登记路径>。

    任何映射都必须仍然落在 root（realpath 之后）之内。
    """

    def __init__(self, root, single_id=None, registry=None):
        self.root = os.path.abspath(root)
        self.root_real = os.path.realpath(self.root)
        self.single_id = single_id
        self.registry = dict(registry or {})

    def describe(self):
        if self.single_id is not None:
            return f"single workspace {self.single_id!r} -> {self.root}"
        if self.registry:
            return f"workspace root {self.root} (registry={sorted(self.registry)})"
        return f"workspace root {self.root} (workspace_id 作为子目录)"

    def _inside(self, target):
        try:
            real = os.path.realpath(target)
            root = os.path.normcase(self.root_real)
            real_n = os.path.normcase(real)
            return os.path.commonpath([root, real_n]) == root
        except (ValueError, OSError):
            # Windows 跨盘符时 commonpath 抛 ValueError；这本身就是越界。
            return False

    def resolve(self, workspace_id):
        wid = workspace_id if isinstance(workspace_id, str) else ""
        if self.single_id is not None:
            if wid != self.single_id:
                raise WorkspaceError(
                    f"no workspace mapping for {wid!r}: 该 worker 固定服务 "
                    f"{self.single_id!r}（{self.root}）；多工作区请用 "
                    "--workspace-root/--workspace-registry"
                )
            target = self.root
        else:
            rel = self.registry.get(wid)
            if rel is None:
                parts = _safe_parts(wid)
                if len(parts) != 1:
                    raise WorkspaceError(
                        f"workspace_id {wid!r} 必须是单个安全目录名，"
                        "跨目录请登记到 --workspace-registry"
                    )
                rel = parts[0]
            target = os.path.join(self.root, rel)
        if not self._inside(target):
            raise WorkspaceError(f"workspace path escapes root: {target!r}")
        return os.path.realpath(target)


class Worker:
    def __init__(self, psql, dsn, worker, ui, resolver, extra, sync_events):
        self.psql = psql
        self.dsn = dsn
        self.worker = worker
        self.ui = ui
        self.resolver = resolver
        self.extra = extra
        self.sync = sync_events
        self.stop = threading.Event()
        self.cancel = threading.Event()
        self.lease_lost = threading.Event()
        self.lease_lost_reason = None

    # ---------- psql 协议 ----------

    def _psql(self, args, stdin_text=None):
        p = subprocess.run(
            [
                self.psql,
                self.dsn,
                "-At",
                "-q",
                "-v",
                "ON_ERROR_STOP=1",
            ]
            + args,
            input=stdin_text,
            text=True,
            capture_output=True,
        )
        if p.returncode != 0:
            raise RuntimeError(p.stderr.strip() or "psql failed")
        return p.stdout.strip()

    def psql_run(self, sql):
        return self._psql(["-c", sql])

    def psql_stdin(self, sql):
        """SQL 走 stdin（`-f -`），长度只受内存限制，不受命令行参数上限影响。"""
        return self._psql(["-f", "-"], stdin_text=sql)

    def claim(self):
        out = self.psql_run(CLAIM_JSON.format(worker=q(self.worker)))
        try:
            rows = json.loads(out or "[]")
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"bad claim json: {exc}: {out[:200]!r}")
        if not isinstance(rows, list):
            raise RuntimeError(f"bad claim json (not a list): {out[:200]!r}")
        if not rows:
            return None
        if len(rows) > 1:
            raise RuntimeError(f"claim returned {len(rows)} rows, expected at most 1")
        row = rows[0]
        if not isinstance(row, dict):
            raise RuntimeError("bad claim row: not an object")
        for key, typ in (
            ("task_seq", int),
            ("session", str),
            ("workspace_id", str),
        ):
            if not isinstance(row.get(key), typ):
                raise RuntimeError(f"bad claim row field {key}: {row.get(key)!r}")
        if row.get("prompt") is not None and not isinstance(row["prompt"], str):
            raise RuntimeError("bad claim row field prompt")
        return row

    def cancel_requested(self, workspace_id, session, seq):
        out = self.psql_run(
            "select cancel_requested from ui_task_records "
            f"where workspace_id={lit(workspace_id)} "
            f"and session_id={lit(session)} and task_seq={int(seq)}"
        )
        return out == "t"

    # ---------- heartbeat / lease ----------

    def _lose_lease(self, reason):
        self.lease_lost_reason = reason
        self.lease_lost.set()
        print(reason, file=sys.stderr)

    def heartbeat(self, workspace_id, session, seq):
        """2s 一次续租；同时轮询取消。

        任务行更新失败、工作区租约被别人拿走、或数据库直接报错，都按
        lease-lost 处理：本地子进程可能还在写 workspace，必须尽快停掉。
        """
        while not self.stop.is_set():
            try:
                out = self.psql_run(
                    "select json_build_object("
                    "'lease', ui_heartbeat_task("
                    + lit(workspace_id)
                    + ","
                    + lit(session)
                    + ","
                    + str(int(seq))
                    + ","
                    + lit(self.worker)
                    + "),"
                    "'cancel_requested', (select cancel_requested from ui_task_records "
                    f"where workspace_id={lit(workspace_id)} "
                    f"and session_id={lit(session)} and task_seq={int(seq)})"
                    ")::text"
                )
                row = json.loads(out or "null")
                if not isinstance(row, dict):
                    raise RuntimeError("bad heartbeat row: " + out[:200])
                if row.get("cancel_requested") is True:
                    self.cancel.set()
                    return
                if row.get("lease") is not True:
                    self._lose_lease(
                        f"lease lost task={seq} session={session} workspace={workspace_id}"
                    )
                    return
            except Exception as exc:
                self._lose_lease(f"lease lost task={seq}: heartbeat failed: {exc}")
                return
            if self.stop.wait(HEARTBEAT_SECONDS):
                return

    # ---------- 事件同步 ----------

    def sync_events(self, workspace_id, session, task_ws):
        """把本地 journal 中 seq 更大的事件增量写进 Postgres。

        用 INSERT ... ON CONFLICT DO NOTHING，重复导入安全；不依赖 COPY。
        整条 SQL 从 stdin 传给 psql，避免大 JSON 撞上命令行参数上限。
        """
        if session == "default":
            path = os.path.join(task_ws, ".ui", "journal.jsonl")
        else:
            path = os.path.join(task_ws, ".ui", "sessions", session + ".jsonl")
        if not os.path.exists(path):
            return 0
        max_seq = self.psql_run(
            "select coalesce(max(seq),0) from ui_events "
            f"where workspace_id={lit(workspace_id)} and session_id={lit(session)}"
        )
        since = int(max_seq or 0)
        vals = []
        with open(path, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                e = json.loads(line)
                if int(e.get("seq", 0)) <= since:
                    continue
                ts = dt.datetime.fromtimestamp(
                    e["ts"] / 1000, tz=dt.timezone.utc
                ).isoformat()
                payload = json.dumps(
                    e, ensure_ascii=False, sort_keys=True, separators=(",", ":")
                )
                actor = e.get("actor") or e.get("actorId")
                vals.append(
                    "("
                    + ",".join(
                        [
                            lit(workspace_id),
                            lit(session),
                            str(int(e["seq"])),
                            lit(e["kind"]),
                            lit(actor),
                            "NULL" if e.get("turn") is None else str(int(e["turn"])),
                            lit(payload),
                            lit(e.get("prev", "")),
                            lit(e["hash"]),
                            lit(ts),
                        ]
                    )
                    + ")"
                )
        if not vals:
            return 0
        self.psql_stdin(
            "insert into ui_events "
            "(workspace_id,session_id,seq,kind,actor,turn,payload,prev_hash,hash,created_at) values "
            + ",".join(vals)
            + " on conflict (workspace_id,session_id,seq) do nothing;\n"
            "select ui_sync_derived();\n"
        )
        return len(vals)

    # ---------- 状态机 ----------

    def release_idle_workspaces(self):
        """释放本 worker 名下已经没有 running 任务的工作区租约。

        heartbeat 会续租；任务终态后这里把租约清掉，别的 worker 才能接手同一
        工作区。失败只记日志：租约最多 60s 自然过期。
        """
        try:
            self.psql_run(
                "delete from ui_workspace_leases wl "
                f"where wl.worker_id={lit(self.worker)} "
                "and not exists (select 1 from ui_task_records t "
                f"where t.workspace_id=wl.workspace_id "
                f"and t.worker_id={lit(self.worker)} and t.status='running')"
            )
        except Exception as exc:
            print(f"workspace lease release failed: {exc}", file=sys.stderr)

    def reap_cancelled(self):
        """Mark tasks already assigned to this worker as cancelled.

        This is called before every claim.  It covers a worker restart (or a
        cancellation requested between claims) where no process is running but
        the row is still ``running`` with ``cancel_requested=true``.
        """
        out = self.psql_run(
            "update ui_task_records "
            "set status='cancelled', finished_at=now(), lease_until=null, last_error=null "
            f"where worker_id={lit(self.worker)} and status='running' and cancel_requested "
            "returning task_seq"
        )
        seqs = [line for line in out.splitlines() if line]
        for seq in seqs:
            print(f"cancelled stale task={seq} worker={self.worker}")
        return len(seqs)

    def mark_cancelled(self, workspace_id, session, seq):
        self.psql_run(
            "update ui_task_records "
            "set status='cancelled', cancel_requested=true, finished_at=now(), "
            "lease_until=null, last_error=null "
            f"where workspace_id={lit(workspace_id)} and session_id={lit(session)} "
            f"and task_seq={int(seq)} and worker_id={lit(self.worker)} "
            "and status='running'"
        )

    def finish(self, workspace_id, session, seq, status, error=None):
        if status == "done":
            # The final guard makes the cancel-vs-complete race safe: a task
            # whose cancellation landed just before we write done is not done.
            out = self.psql_run(
                "update ui_task_records "
                "set status='done', finished_at=now(), lease_until=null, last_error=null "
                f"where workspace_id={lit(workspace_id)} and session_id={lit(session)} "
                f"and task_seq={int(seq)} and worker_id={lit(self.worker)} "
                "and status='running' and not cancel_requested "
                "returning task_seq"
            )
            if out:
                return
            state = self.psql_run(
                "select json_build_object('status', status, "
                "'cancel_requested', cancel_requested)::text "
                "from ui_task_records "
                f"where workspace_id={lit(workspace_id)} and session_id={lit(session)} "
                f"and task_seq={int(seq)}"
            )
            if not state:
                print(f"could not finish task={seq}: row disappeared", file=sys.stderr)
                return
            try:
                row = json.loads(state)
            except json.JSONDecodeError:
                print(f"could not finish task={seq}: bad state {state!r}", file=sys.stderr)
                return
            if isinstance(row, dict) and row.get("cancel_requested") is True:
                self.mark_cancelled(workspace_id, session, seq)
                return
            if isinstance(row, dict) and row.get("status") == "done":
                # ui_sync_derived() may have observed turn/end before us.
                return
            print(f"could not finish task={seq}: lease/status changed", file=sys.stderr)
            return
        if status == "cancelled":
            self.mark_cancelled(workspace_id, session, seq)
            return
        text = "" if error is None else str(error)
        out = self.psql_run(
            "update ui_task_records "
            "set status='failed', finished_at=now(), lease_until=null, "
            f"last_error={lit(text[:ERROR_MAX])} "
            f"where workspace_id={lit(workspace_id)} and session_id={lit(session)} "
            f"and task_seq={int(seq)} and worker_id={lit(self.worker)} "
            "and status='running' "
            "returning task_seq"
        )
        if not out:
            print(
                f"could not mark task={seq} failed: lease/status changed",
                file=sys.stderr,
            )

    @staticmethod
    def terminate(proc):
        if proc.poll() is not None:
            return
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass

    # ---------- 单个任务 ----------

    def one(self):
        reaped = self.reap_cancelled()
        claim = self.claim()
        if claim is None:
            self.release_idle_workspaces()
            return reaped > 0

        seq = claim["task_seq"]
        seq_i = int(seq)
        session = claim["session"]
        workspace_id = claim["workspace_id"]
        prompt = claim.get("prompt") or ""

        # A cancellation may have raced with the claim.  Do not launch ui for it.
        if claim.get("cancel_requested") is True or self.cancel_requested(
            workspace_id, session, seq_i
        ):
            self.mark_cancelled(workspace_id, session, seq_i)
            self.release_idle_workspaces()
            print(f"cancelled task={seq} session={session} before run")
            return True

        # 映射失败是终态失败，不是"跳过"：原地重试会无限失败，换目录跑更糟。
        try:
            if not valid_session(session):
                raise SessionError(
                    f"invalid session id {session!r}: 只允许 [A-Za-z0-9_-]，最长 64"
                )
            task_ws = self.resolver.resolve(workspace_id)
            os.makedirs(task_ws, exist_ok=True)
        except (WorkspaceError, SessionError, OSError) as exc:
            msg = f"workspace/session mapping refused: {exc}"
            print(f"task={seq} {msg}", file=sys.stderr)
            self.finish(workspace_id, session, seq_i, "failed", error=msg)
            self.release_idle_workspaces()
            return True

        self.stop = threading.Event()
        self.cancel = threading.Event()
        self.lease_lost = threading.Event()
        self.lease_lost_reason = None
        hb = threading.Thread(
            target=self.heartbeat,
            args=(workspace_id, session, seq_i),
            daemon=True,
        )
        hb.start()
        status = "failed"
        error = None
        try:
            proc = subprocess.Popen(
                [
                    self.ui,
                    "run",
                    "--workspace",
                    task_ws,
                    "--session",
                    session,
                    "--task",
                    prompt,
                    "--yes",
                ]
                + self.extra,
                text=True,
            )
            while True:
                # lease 先于 cancel 检查：租约没了就没有资格写任何终态。
                if self.lease_lost.is_set():
                    error = self.lease_lost_reason or "lease lost"
                    self.terminate(proc)
                    break
                if self.cancel.is_set():
                    status = "cancelled"
                    self.terminate(proc)
                    break
                rc = proc.poll()
                if rc is not None:
                    if rc == 0:
                        status = "done"
                    else:
                        status = "failed"
                        error = f"ui exited with {rc}"
                    break
                time.sleep(UI_POLL_SECONDS)
            # heartbeat 可能在 rc 观测之后才置位；落终态前再确认一次。
            if self.lease_lost.is_set():
                status = "failed"
                error = self.lease_lost_reason or "lease lost"
            if status == "done" and (
                self.cancel.is_set()
                or self.cancel_requested(workspace_id, session, seq_i)
            ):
                status = "cancelled"
        except Exception as exc:
            print(f"task {seq} failed: {exc}", file=sys.stderr)
            status = "failed"
            error = f"worker error: {exc}"
        finally:
            self.stop.set()
            hb.join(timeout=5)

        lease_lost = self.lease_lost.is_set()
        if self.sync and not lease_lost:
            try:
                n = self.sync_events(workspace_id, session, task_ws)
                print(f"synced {n} new events task={seq}")
            except Exception as exc:
                print(f"sync failed task={seq}: {exc}", file=sys.stderr)
                if status != "cancelled":
                    status = "failed"
                    error = f"sync failed: {exc}"
        elif self.sync and lease_lost:
            # 工作区可能已经被别的 worker 接手；把本地事件同步上去会让
            # ui_sync_derived() 按 turn/end 改状态，等于替别人写 done。
            print(f"skip sync task={seq}: lease lost", file=sys.stderr)

        self.finish(workspace_id, session, seq_i, status, error=error)
        self.release_idle_workspaces()
        print(f"{status} task={seq} session={session} workspace={workspace_id}")
        return True


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--psql", default="psql")
    p.add_argument("--dsn", default=os.environ.get("DATABASE_URL", ""))
    p.add_argument("--worker", default="worker-1")
    p.add_argument("--ui", required=True)
    # 多工作区：root + 每个 workspace_id 的子目录（或 registry 登记路径）。
    p.add_argument("--workspace-root", default=None)
    # 单工作区兼容：这个目录只服务 --workspace-id 一个 workspace_id。
    p.add_argument("--workspace", default=None)
    p.add_argument("--workspace-id", default=None)
    p.add_argument("--workspace-registry", default=None)
    p.add_argument("--once", action="store_true")
    p.add_argument("--sync-events", action="store_true")
    p.add_argument("--poll", type=float, default=2.0)
    p.add_argument("extra", nargs=argparse.REMAINDER)
    a = p.parse_args()

    if not a.dsn:
        raise SystemExit("需要 --dsn 或 DATABASE_URL")
    if bool(a.workspace_root) == bool(a.workspace):
        raise SystemExit("必须且只能给 --workspace-root（多工作区）或 --workspace（单工作区）之一")
    if a.workspace_id is not None and a.workspace is None:
        raise SystemExit("--workspace-id 只在单工作区模式（--workspace）下有效")
    if a.workspace_registry is not None and a.workspace_root is None:
        raise SystemExit("--workspace-registry 只在 --workspace-root 模式下有效")
    registry = load_registry(a.workspace_registry) if a.workspace_registry else {}
    single_id = None
    if a.workspace is not None:
        single_id = a.workspace_id or "default"
    root = a.workspace_root if a.workspace_root is not None else a.workspace
    if not os.path.isdir(root):
        raise SystemExit(f"workspace 根目录不存在或不是目录: {root}")
    resolver = WorkspaceResolver(root, single_id=single_id, registry=registry)

    extra = [x for x in a.extra if x != "--"]
    for item in extra:
        for key in RESERVED_UI_OPTIONS:
            if item == key or item.startswith(key + "="):
                raise SystemExit(
                    f"extra 里不能出现 {key}：--workspace/--session 由 worker 从 "
                    "claim 结果映射，重复传会覆盖映射"
                )

    w = Worker(
        a.psql,
        a.dsn,
        a.worker,
        a.ui,
        resolver,
        extra,
        a.sync_events,
    )
    print(f"worker {a.worker}: {resolver.describe()}", file=sys.stderr)
    if a.once:
        return 0 if w.one() else 1
    while True:
        try:
            if not w.one():
                time.sleep(a.poll)
        except KeyboardInterrupt:
            return 0
        except Exception as exc:
            print(f"worker error: {exc}", file=sys.stderr)
            time.sleep(a.poll)


if __name__ == "__main__":
    sys.exit(main())
