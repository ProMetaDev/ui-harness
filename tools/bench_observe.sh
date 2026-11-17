#!/usr/bin/env bash
# 同一进程内 N 次 shell 调用：三种模式的成本（N 由参数决定，标签自动带 N）
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
export PATH="$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:$PATH"
cd "$UI_ROOT"

N=${1:-100}
WS=/tmp/ui-obs-bench3
rm -rf "$WS"; mkdir -p "$WS"
python3 - "$WS" "$N" <<'PY'
import os, sys, json
root, n = sys.argv[1], int(sys.argv[2])
for i in range(2000):
    d = os.path.join(root, f"d{i//100}")
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, f"f{i}.txt"), "w").write("x" * 100)
steps = [{"content": f"run {i}", "toolCalls": [
    {"name": "shell", "args": {"command": f"echo {i} > probe{i}.txt"}}]} for i in range(n)]
steps.append({"content": "done"})
json.dump({"model": "scripted", "steps": steps}, open(os.path.join(root, "sc.json"), "w"))
PY

measure() {
  local label="$1"; shift
  local times=()
  for _ in 1 2 3; do
    local t0=$(date +%s%3N)
    "$@" >/dev/null 2>&1
    times+=($(( $(date +%s%3N) - t0 )))
  done
  local sorted=($(printf '%s\n' "${times[@]}" | sort -n))
  printf '%-24s %6s ms  → %s ms/次\n' "$label" "${sorted[1]}" "$(( sorted[1] / N ))"
}

echo "工作区 $(find "$WS" -type f | wc -l) 个文件 · $N 次 shell 调用"
measure "inotify" env UI_OBSERVE=1 UI_OBSERVE_SCAN=0 "$UI_BIN" run --task b --mock "$WS/sc.json" --workspace "$WS" --yes --journal "$WS/.ui/a.jsonl"
measure "全量扫描" env UI_OBSERVE=1 UI_OBSERVE_SCAN=1 "$UI_BIN" run --task b --mock "$WS/sc.json" --workspace "$WS" --yes --journal "$WS/.ui/b.jsonl"
measure "关闭观测" env UI_OBSERVE=0 "$UI_BIN" run --task b --mock "$WS/sc.json" --workspace "$WS" --yes --journal "$WS/.ui/c.jsonl"
