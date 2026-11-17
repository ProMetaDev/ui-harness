#!/usr/bin/env bash
# 性能基线：把"快不快"变成数字，改之前先量，改完再量。
#
#   bash tools/bench.sh
#
# 指标：墙钟中位数（5 次）+ 峰值 RSS。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

ROOT=${BENCH_ROOT:-/tmp/ui-bench}
rm -rf "$ROOT"; mkdir -p "$ROOT"
ROW="$ROOT/rows.txt"
: > "$ROW"

NOW_MS() { date +%s%3N; }

# 跑 N 次取中位数（毫秒）与峰值 RSS（kB）
measure() {
  local label="$1"; shift
  local -a times=()
  local peak=0
  for _ in 1 2 3 4 5; do
    local t0=$(NOW_MS)
    "$@" >/dev/null 2>&1 &
    local pid=$!
    local p=0
    while kill -0 "$pid" 2>/dev/null; do
      local r=$(awk '/VmRSS/{print $2}' "/proc/$pid/status" 2>/dev/null)
      [ -n "$r" ] && [ "$r" -gt "$p" ] && p=$r
      sleep 0.005
    done
    wait "$pid"
    local t1=$(NOW_MS)
    times+=($((t1 - t0)))
    [ "$p" -gt "$peak" ] && peak=$p
  done
  local sorted=($(printf '%s\n' "${times[@]}" | sort -n))
  local med=${sorted[2]}
  printf '%-38s %7s ms %10s kB\n' "$label" "$med" "$peak" | tee -a "$ROW"
}

# 造一份指定事件数的合法日志：用脚本模型跑足够多步
make_journal() {
  local ws="$1" steps="$2"
  python3 - "$ws/sc.json" "$steps" <<'PY'
import json, sys
path, n = sys.argv[1], int(sys.argv[2])
steps = []
for i in range(n):
    steps.append({"content": f"s{i}", "toolCalls": [
        {"name": "fs_write", "args": {"path": "x.txt", "content": f"v{i}"}}]})
steps.append({"content": "done"})
json.dump({"model": "scripted", "steps": steps}, open(path, "w"))
PY
  "$UI_BIN" run --task bench --mock "$ws/sc.json" --workspace "$ws" --yes \
    --max-steps $((steps * 2 + 10)) --max-calls $((steps * 3 + 10)) \
    --max-tokens 100000000 --max-wall-ms 600000 >/dev/null 2>&1
}

echo "== 冷启动与轻量命令 =="
measure "ui --version" "$UI_BIN" --version
measure "ui doctor" "$UI_BIN" doctor --workspace demo
measure "ui policy（单条判定）" "$UI_BIN" policy --effect fs.write --target /tmp/x

echo
echo "== 一轮对话（mock，1 步）=="
WS1="$ROOT/turn"; mkdir -p "$WS1"
cat > "$WS1/sc.json" <<'JSON'
{"model":"scripted","steps":[{"content":"写","toolCalls":[{"name":"fs_write","args":{"path":"a.txt","content":"x"}}]},{"content":"完成"}]}
JSON
measure "ui run（1 步 1 工具）" "$UI_BIN" run --task b --mock "$WS1/sc.json" --workspace "$WS1" --yes --journal "$WS1/.ui/j.jsonl"

echo
echo "== 日志规模对 verify / replay 的影响 =="
for steps in 200 1000; do
  ws="$ROOT/j$steps"; mkdir -p "$ws"
  make_journal "$ws" "$steps"
  n=$(wc -l < "$ws/.ui/journal.jsonl")
  sz=$(du -h "$ws/.ui/journal.jsonl" | cut -f1)
  echo "  （$n 条事件 / $sz）"
  measure "ui verify（$n 条）" "$UI_BIN" verify --workspace "$ws"
  measure "ui replay（$n 条，输出丢弃）" "$UI_BIN" replay --workspace "$ws"
done

echo
echo "== 观测与沙箱的每次开销 =="
WS2="$ROOT/obs"; mkdir -p "$WS2"
python3 - "$WS2" <<'PY'
import os, sys
root = sys.argv[1]
for i in range(2000):
    d = os.path.join(root, f"d{i//100}")
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, f"f{i}.txt"), "w").write("x" * 100)
PY
cat > "$WS2/sc.json" <<'JSON'
{"model":"scripted","steps":[{"content":"跑","toolCalls":[{"name":"shell","args":{"command":"echo hi"}}]},{"content":"完成"}]}
JSON
measure "ui run（2000 文件 + 观测开）" env UI_OBSERVE=1 "$UI_BIN" run --task b --mock "$WS2/sc.json" --workspace "$WS2" --yes --journal "$WS2/a.jsonl"
measure "ui run（2000 文件 + 观测关）" env UI_OBSERVE=0 "$UI_BIN" run --task b --mock "$WS2/sc.json" --workspace "$WS2" --yes --journal "$WS2/b.jsonl"

echo
echo "== 实验打包（2000 文件工作区）=="
measure "ui lab pack" "$UI_BIN" lab pack --name b --workspace "$WS2" --out "$ROOT/bundle"

echo
echo "结果已写入 $ROW"
