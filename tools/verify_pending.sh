#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 验证两件"没做完"的事：
#   1) ui run 在 empty_reply 这类非 done 原因下，退出码必须非 0
#   2) 符号链接能否逃出工作区（对抗 agent 报的未修项）
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
BASE=/tmp/ui-check2
rm -rf "$BASE"; mkdir -p "$BASE/work"

cat > "$BASE/work/empty.json" <<'EOF'
{"model":"scripted","steps":[{"content":""},{"content":""},{"content":""}]}
EOF
"$BIN" run --task "说点什么" --mock "$BASE/work/empty.json" --workspace "$BASE/work" --yes >/dev/null 2>&1
echo "empty_reply 退出码: $?  （应该是 1）"

cat > "$BASE/work/ok.json" <<'EOF'
{"model":"scripted","steps":[{"content":"好了"}]}
EOF
"$BIN" run --task "说点什么" --mock "$BASE/work/ok.json" --workspace "$BASE/work" --yes >/dev/null 2>&1
echo "done 退出码: $?  （应该是 0）"

# 符号链接逃逸
echo "TOP-SECRET" > "$BASE/secret.txt"
ln -sf ../secret.txt "$BASE/work/link.txt"
cat > "$BASE/work/link.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"读链接","toolCalls":[{"name":"fs_read","args":{"path":"link.txt"}}]},
  {"content":"结束"}]}
EOF
"$BIN" run --task "读链接" --mock "$BASE/work/link.json" --workspace "$BASE/work" --yes 2>&1 | grep -E '→|结束原因' || true
echo "日志里 link.txt 的决策: $(grep -o '"decision":"[a-z]*"' "$BASE/work/.ui/journal.jsonl" 2>/dev/null | tail -1)"
grep -q 'TOP-SECRET' "$BASE/work/.ui/journal.jsonl" 2>/dev/null \
  && echo "→ 泄露：工作区外内容进了日志/上下文（符号链接逃逸仍在）" \
  || echo "→ 没有泄露"
