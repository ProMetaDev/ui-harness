#!/usr/bin/env python3
"""最小 Postgres worker：claim -> run ui -> heartbeat -> done/failed。

只使用 Python 标准库 + 可选 psql（L2 adapter）。不进入 ui-core。
"""
import argparse
import os
import subprocess
import sys
import threading
import time

CLAIM = """
select c.task_seq, c.session_id, c.workspace_id, t.prompt
from ui_claim_task('{worker}') c
join ui_task_records t
  on t.workspace_id=c.workspace_id and t.session_id=c.session_id and t.task_seq=c.task_seq
"""


def q(sql):
    return sql.replace("'", "''")


class Worker:
    def __init__(self, psql, dsn, worker, ui, workspace, extra):
        self.psql = psql
        self.dsn = dsn
        self.worker = worker
        self.ui = ui
        self.workspace = workspace
        self.extra = extra
        self.stop = threading.Event()

    def psql_run(self, sql):
        p = subprocess.run(
            [self.psql, self.dsn, "-At", "-F", "\t", "-v", "ON_ERROR_STOP=1", "-c", sql],
            text=True,
            capture_output=True,
        )
        if p.returncode != 0:
            raise RuntimeError(p.stderr.strip() or "psql failed")
        return p.stdout.strip()

    def heartbeat(self, workspace_id, session, seq):
        while not self.stop.wait(20):
            try:
                self.psql_run(
                    f"select ui_heartbeat_task('{q(workspace_id)}','{q(session)}',{seq},'{q(self.worker)}')"
                )
            except Exception as exc:
                print(f"heartbeat failed: {exc}", file=sys.stderr)
                return

    def one(self):
        out = self.psql_run(CLAIM.format(worker=q(self.worker)))
        if not out:
            return False
        parts = out.split("\t")
        if len(parts) < 4:
            raise RuntimeError("bad claim row: " + out)
        seq, session, workspace_id, prompt = parts[0], parts[1], parts[2], parts[3]
        hb = threading.Thread(target=self.heartbeat, args=(workspace_id, session, int(seq)), daemon=True)
        hb.start()
        try:
            p = subprocess.run(
                [self.ui, "run", "--workspace", self.workspace, "--task", prompt, "--yes"]
                + self.extra,
                text=True,
            )
            if p.returncode != 0:
                raise RuntimeError(f"ui exited {p.returncode}")
            status = "done"
        except Exception as exc:
            print(f"task {seq} failed: {exc}", file=sys.stderr)
            status = "failed"
        finally:
            self.stop.set()
        extra = ", finished_at=now()" if status in ("done", "failed") else ""
        self.psql_run(
            f"update ui_task_records set status='{status}'{extra} "
            f"where workspace_id='{q(workspace_id)}' and session_id='{q(session)}' and task_seq={int(seq)}"
        )
        print(f"{status} task={seq} session={session}")
        return True


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--psql", default="psql")
    p.add_argument("--dsn", default=os.environ.get("DATABASE_URL", ""))
    p.add_argument("--worker", default="worker-1")
    p.add_argument("--ui", required=True)
    p.add_argument("--workspace", required=True)
    p.add_argument("--once", action="store_true")
    p.add_argument("--poll", type=float, default=2.0)
    p.add_argument("extra", nargs=argparse.REMAINDER)
    a = p.parse_args()
    if not a.dsn:
        raise SystemExit("需要 --dsn 或 DATABASE_URL")
    extra = [x for x in a.extra if x != "--"]
    w = Worker(a.psql, a.dsn, a.worker, a.ui, a.workspace, extra)
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
