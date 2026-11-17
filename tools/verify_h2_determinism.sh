#!/usr/bin/env bash
# 实验 H2：确定性检索 —— 同一输入重复运行，注入集合必须完全一致。
#
# 若任意一次不同，说明检索有隐状态（缓存/顺序/时间），P6 不成立 → P2（注入可复现）也不成立。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-h2
rm -rf "$WS"; mkdir -p "$WS"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[{"content":"完成"}]}
JSON

# 写三条记忆，其中两条与任务相关
"$UI_BIN" remember "CI 门禁必须在本机先跑 debug.sh" --kind pitfall --key gate --workspace "$WS" >/dev/null
"$UI_BIN" remember "提交前先跑 bash tools/test.sh" --kind build --key test --workspace "$WS" >/dev/null
"$UI_BIN" remember "周三发布窗口在下午" --kind fact --key release --workspace "$WS" >/dev/null

ids=""
for i in $(seq 1 20); do
  rm -f "$WS/.ui/journal.jsonl"
  "$UI_BIN" run --task "跑一遍门禁再提交" --mock "$WS/sc.json" --workspace "$WS" --yes \
    --journal "$WS/.ui/journal.jsonl" >/dev/null 2>&1
  seq_ids=$(python3 - "$WS/.ui/journal.jsonl" <<'PY'
import json, sys
out = []
for line in open(sys.argv[1], encoding="utf-8"):
    e = json.loads(line)
    if e.get("kind") == "memory/used":
        out.append(e.get("id", "?"))
print(",".join(out))
PY
)
  ids="$ids|$seq_ids"
done

uniq_count=$(printf '%s' "$ids" | tr '|' '\n' | grep -c . )
distinct=$(printf '%s' "$ids" | tr '|' '\n' | grep . | sort -u | wc -l)

echo "20 次运行的注入序列（每次一行）："
printf '%s' "$ids" | tr '|' '\n' | grep . | nl | head -20
echo
echo "不同序列数: $distinct"
if [ "$distinct" -eq 1 ]; then
  echo "PASS H2：确定性检索成立——20 次注入完全一致"
else
  echo "FAIL H2：检索有隐状态（P6 不成立，P2 也就不可声称）"
  exit 1
fi
