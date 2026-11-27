#!/usr/bin/env bash
# 实验：验证「观测是下界，不是等价」。
#
# 让 shell 在工作区**外面**写一个文件。日志里不该出现 effect/observed——
# 不是因为我们记得对，而是因为观测范围只有工作区。
# 这正是"日志里没有 change ≠ 什么都没发生"的实证。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-obs-bound
OUT=/tmp/ui-obs-bound-outside.txt
rm -rf "$WS" "$OUT"; mkdir -p "$WS"

cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"往工作区外写","toolCalls":[{"name":"shell","args":{"command":"echo leaked > /tmp/ui-obs-bound-outside.txt"}}]},
  {"content":"完成"}]}
JSON

UI_SANDBOX=0 "$UI_BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes >/dev/null 2>&1
# 这个实验要观察“策略观测不到、但 OS 没拦住时确实会发生区外写入”。
# 开启读侧/写入沙箱后该命令会被 Landlock 拒绝，那就无法证明观测下界；
# 因而只在本实验内关闭沙箱，不改变产品默认值。
observed=$(grep -c '"kind":"effect/observed"' "$WS/.ui/journal.jsonl" || true)
echo "工作区外的文件是否真的被写了: $([ -f "$OUT" ] && echo 是 || echo 否)"
echo "日志里 effect/observed 事件数: $observed"
echo

if [ -f "$OUT" ] && [ "$observed" -eq 0 ]; then
  echo "PASS 结论 A 得证：确实发生了工作区外的写入，而日志里没有任何 observed 记录"
  echo "     → 唯一正确的表述是「观测 ⊆ 实际」，不是「没记录 = 没发生」"
else
  echo "FAIL 预期不成立（文件存在=$([ -f "$OUT" ] && echo y || echo n)，observed=$observed）"
  exit 1
fi
rm -f "$OUT"
