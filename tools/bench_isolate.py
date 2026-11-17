#!/usr/bin/env python3
"""隔离副本基准：用 monotonic 计时（WSL2 的墙钟会被时间同步校正，会出负值）。

用法：python3 tools/bench_isolate.py [基准二进制路径]
"""
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

ROOT = "/tmp/ui-isolate-bench"
AGENTS = os.path.join(ROOT, ".ui", "agents")


def ensure_workspace() -> None:
    if os.path.isfile(os.path.join(ROOT, "plan.json")):
        return
    shutil.rmtree(ROOT, ignore_errors=True)
    for i in range(8000):
        d = os.path.join(ROOT, "src", f"d{i // 400}")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, f"f{i}.py"), "w") as f:
            f.write("x" * 10240)
    plan = {
        "model": "scripted",
        "steps": [
            {"content": '{"parallel":[{"prompt":"a"},{"prompt":"b"},{"prompt":"c"}],"summary":"汇总"}'},
            {"content": "done"},
            {"content": "done"},
            {"content": "done"},
            {"content": "汇总"},
        ],
    }
    with open(os.path.join(ROOT, "plan.json"), "w") as f:
        json.dump(plan, f)


def run(binary: str) -> float:
    shutil.rmtree(AGENTS, ignore_errors=True)
    t0 = time.monotonic()
    subprocess.run(
        [
            binary, "team", "--task", "t", "--mock", os.path.join(ROOT, "plan.json"),
            "--workspace", ROOT, "--isolate", "--yes", "--agents", "3",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return (time.monotonic() - t0) * 1000.0


def bench(label: str, binary: str, n: int = 5) -> None:
    times = [run(binary) for _ in range(n)]
    nice = ", ".join(f"{t:.0f}" for t in times)
    print(f"{label}  各次: {nice}  中位数 {statistics.median(times):.0f} ms")


def main() -> int:
    ensure_workspace()
    files = sum(len(fs) for _, _, fs in os.walk(ROOT) if ".ui" not in _)
    print(f"工作区: {files} 个文件")

    current = os.environ.get("UI_BIN") or os.path.join(
        os.path.expanduser("~"), "ui-target", "release", "ui"
    )
    if not (current and os.path.exists(current)):
        print(f"找不到当前二进制: {current!r}")
        return 1

    bench("当前版本", current)

    base = sys.argv[1] if len(sys.argv) > 1 else ""
    if base:
        if not os.path.exists(base):
            print(f"找不到基准二进制: {base!r}")
            return 1
        bench("基准版本", base)
    return 0


if __name__ == "__main__":
    sys.exit(main())
