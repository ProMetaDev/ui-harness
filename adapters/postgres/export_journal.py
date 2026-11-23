#!/usr/bin/env python3
"""把 ui journal.jsonl 导出成 Postgres COPY 文本（L2 adapter）。

只使用 Python 标准库；不连接数据库，不解析/改写 hash。导入由 psql 完成。
"""
import argparse
import datetime as dt
import json
import sys

COLS = (
    "workspace_id",
    "session_id",
    "seq",
    "kind",
    "actor",
    "turn",
    "payload",
    "prev_hash",
    "hash",
    "created_at",
)


def copy_field(value):
    if value is None:
        return r"\N"
    text = str(value)
    return (
        text.replace("\\", "\\\\")
        .replace("\t", "\\t")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
    )


def iso(ts_ms):
    # journal ts 是毫秒；没有就拒绝，不能静默写 now()。
    if not isinstance(ts_ms, (int, float)):
        raise ValueError("event 缺少 ts")
    return dt.datetime.fromtimestamp(ts_ms / 1000, tz=dt.timezone.utc).isoformat()


def export(path, session, workspace, out):
    out.write("COPY ui_events (" + ", ".join(COLS) + ") FROM STDIN;\n")
    n = 0
    with open(path, "r", encoding="utf-8") as f:
        for lineno, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                e = json.loads(line)
                row = [
                    workspace,
                    session,
                    e["seq"],
                    e["kind"],
                    e.get("actor") or e.get("actorId"),
                    e.get("turn"),
                    json.dumps(e, ensure_ascii=False, sort_keys=True, separators=(",", ":")),
                    e["prev"],
                    e["hash"],
                    iso(e.get("ts")),
                ]
            except Exception as exc:
                raise SystemExit(f"{path}:{lineno}: {exc}")
            out.write("\t".join(copy_field(v) for v in row) + "\n")
            n += 1
    out.write("\\.\n")
    return n


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--journal", required=True)
    p.add_argument("--session", default="default")
    p.add_argument("--workspace", default="default")
    p.add_argument("--output", default="-")
    a = p.parse_args()
    if a.output == "-":
        n = export(a.journal, a.session, a.workspace, sys.stdout)
    else:
        with open(a.output, "w", encoding="utf-8", newline="\n") as f:
            n = export(a.journal, a.session, a.workspace, f)
    print(f"exported {n} events", file=sys.stderr)


if __name__ == "__main__":
    main()
