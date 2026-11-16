#!/usr/bin/env bash
# 独立验证：`..` 路径穿越是否真的被挡住（不看对抗 agent 的测试，自己跑 E2E）。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
BASE=/tmp/ui-escape
rm -rf "$BASE"; mkdir -p "$BASE/work"

echo "TOP-SECRET" > "$BASE/secret.txt"

cat > "$BASE/work/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"读一下外面的文件","toolCalls":[{"name":"fs_read","args":{"path":"../secret.txt"}}]},
  {"content":"再往外面写一个","toolCalls":[{"name":"fs_write","args":{"path":"../pwned.txt","content":"x"}}]},
  {"content":"结束了"}]}
EOF

"$BIN" run --task "试试穿越" --mock "$BASE/work/sc.json" --workspace "$BASE/work" --yes 2>&1 | grep -E '→|结束原因' || true

echo
echo "父目录里有没有被写出 pwned.txt: $(test -f "$BASE/pwned.txt" && echo '有（漏洞仍在）' || echo '没有（已挡住）')"
echo "日志里 fs.read 的决策:"
grep -o '"kind":"fs.read"[^}]*' "$BASE/work/.ui/journal.jsonl" 2>/dev/null | head -2 || true
grep -o '"decision":"[a-z]*"' "$BASE/work/.ui/journal.jsonl" 2>/dev/null | head -4
