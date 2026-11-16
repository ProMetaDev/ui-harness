#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
WS=/tmp/ui-tc; rm -rf "$WS"; mkdir -p "$WS"
python3 - 2>/dev/null <<'EOF' || seq 1 400 | sed 's/^/line /' > /tmp/ui-tc/big.txt
EOF
seq 1 400 | sed 's/^/line /' > "$WS/big.txt"
cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
 {"content":"读","toolCalls":[{"name":"fs_read","args":{"path":"big.txt"}}]},
 {"content":"好了"}]}
EOF
for c in 0 2000; do
  rm -rf "$WS/.ui"
  "$BIN" run --task "读一下" --mock "$WS/sc.json" --workspace "$WS" --yes --tool-chars "$c" >/dev/null 2>&1
  echo "tool-chars=$c → trim 事件 $(grep -c 'tool/trim' "$WS/.ui/journal.jsonl" 2>/dev/null || echo 0)"
done
