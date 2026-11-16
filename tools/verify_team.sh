#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 协作模式：一个复杂任务 → 模型拆成并行子任务 → 隔离工作区并行跑 → 汇总。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-team
rm -rf "$WS"; mkdir -p "$WS"

# 剧本：第一步返回计划；后续每个子代理各自从第 1 步开始，用一个写文件的动作表示干完了
cat > "$WS/plan.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"{\"parallel\":[{\"prompt\":\"写一个模块 A 的草稿\"},{\"prompt\":\"写一个模块 B 的草稿\"}],\"summary\":\"把两份草稿合成一份\"}"},
  {"content":"子任务完成","toolCalls":[{"name":"fs_write","args":{"path":"draft.txt","content":"草稿"}}]},
  {"content":"完成"}]}
EOF

echo "=== 协作模式（2 个子代理，隔离）==="
"$BIN" team --task "把这件事拆开做：写模块 A 和 B" --agents 2 \
  --mock "$WS/plan.json" --workspace "$WS" --yes 2>&1 | tail -24

echo
echo "=== 产生的计划文件 ==="
ls "$WS/.ui"/team-*.json 2>/dev/null | head -2
cat "$WS/.ui"/team-*.json 2>/dev/null | head -20

echo
echo "=== 日志里的工作流事件 ==="
python3 - <<'PY'
import json, glob
p = glob.glob('/tmp/ui-team/.ui/journal.jsonl')
if not p:
    print('没有日志'); raise SystemExit
for line in open(p[0], encoding='utf-8'):
    ev = json.loads(line)
    k = ev.get('kind', '')
    if k.startswith('workflow') or k.startswith('delegate'):
        print(k, {x: ev[x] for x in ('step','agent','reason','ok') if x in ev})
PY
