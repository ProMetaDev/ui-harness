#!/usr/bin/env bash
# 读取 soak state，检查 phase/rounds/心跳是否陈旧。
# 用法: tools/soak_status.sh [state-file]
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
STATE="${1:-${UI_SOAK_STATE:-/tmp/ui-soak-long-state.json}}"
[ -f "$STATE" ] || { echo "FAIL: 没有 state 文件: $STATE"; exit 2; }

python3 - "$STATE" <<'PY'
import json, sys, time
state = sys.argv[1]
d = json.load(open(state, encoding="utf-8"))
now = int(time.time())
age = now - int(d.get("updatedAt", 0))
interval = int(d.get("interval", 60))
phase = d.get("phase")
rounds = d.get("rounds", 0)
print(f"phase={phase} rounds={rounds} age={age}s interval={interval}s state={state}")
if phase == "failed":
    sys.exit(2)
if phase == "passed":
    sys.exit(0)
if phase != "running":
    print("FAIL: 未知 phase", file=sys.stderr)
    sys.exit(2)
# running 但超过 3 个间隔没有心跳：进程可能卡死。
if age > max(interval * 3, 300):
    print(f"FAIL: 心跳过期 ({age}s > {max(interval * 3, 300)}s)", file=sys.stderr)
    sys.exit(1)
PY
