#!/usr/bin/env bash
# 实验：验证「纯哈希链无法检测尾部截断」。
#
# 把日志最后 3 条整行删掉（不是撕裂半行，是干净删除），再跑 ui verify。
# 预期：verify 说"链完整"——因为剩下的仍是一条合法前缀。
# 这正是必须写进理论文档的边界（结论 C）。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-trunc
rm -rf "$WS"; mkdir -p "$WS"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"a","toolCalls":[{"name":"fs_write","args":{"path":"a.txt","content":"1"}}]},
  {"content":"b","toolCalls":[{"name":"fs_write","args":{"path":"b.txt","content":"2"}}]},
  {"content":"c","toolCalls":[{"name":"fs_write","args":{"path":"c.txt","content":"3"}}]},
  {"content":"完成"}]}
JSON
"$UI_BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes >/dev/null 2>&1

J="$WS/.ui/journal.jsonl"
before=$(wc -l < "$J")
echo "原始事件数: $before"

# 干净删除最后 3 行
python3 - "$J" <<'PY'
import sys
p = sys.argv[1]
lines = open(p, encoding="utf-8").read().splitlines()
keep = lines[:-3]
open(p, "w", encoding="utf-8").write("\n".join(keep) + "\n")
PY
after=$(wc -l < "$J")
echo "截断后事件数: $after"

out=$("$UI_BIN" verify --journal "$J" 2>&1); code=$?
echo "verify 退出码: $code"
echo "$out" | head -4

if [ "$code" -eq 0 ]; then
  echo
  echo "PASS 结论 C 得证：干净截断尾部后 verify 仍判「完整」"
  echo "     → 纯哈希链只能保证「前缀一致」，不能保证「没有删掉尾巴」"
else
  echo "FAIL 预期不成立：verify 检出了截断（退出码 $code）"
  exit 1
fi
