#!/usr/bin/env bash
# 真多进程验证：3 个 ui 进程同时往同一个 workspace 的 journal 里追加。
# 期望：链完整、seq 连续、事件数正好是 3 份。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
BASE=/tmp/ui-concurrent
rm -rf "$BASE"; mkdir -p "$BASE/work"

cat > "$BASE/work/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"写一个自己的文件","toolCalls":[{"name":"fs_write","args":{"path":"mine.txt","content":"x"}}]},
  {"content":"好了"}]}
EOF

pids=()
for i in 1 2 3; do
  "$BIN" run --task "第 $i 个进程" --mock "$BASE/work/sc.json" \
    --workspace "$BASE/work" --yes > "$BASE/p$i.log" 2>&1 &
  pids+=($!)
done
for p in "${pids[@]}"; do wait "$p"; done

echo "=== 每个进程的收尾 ==="
for i in 1 2 3; do grep -h '结束原因' "$BASE/p$i.log" | sed "s/^/  p$i: /"; done

echo
echo "=== 日志自检 ==="
"$BIN" verify --workspace "$BASE/work"
echo "verify 退出码: $?"

echo
echo "=== 事件数与 seq 连续性 ==="
COUNT=$(wc -l < "$BASE/work/.ui/journal.jsonl")
echo "事件数: $COUNT"
SEQ_OK=$(bash tools/_node.sh -e '
const fs=require("fs");
const lines=fs.readFileSync("/tmp/ui-concurrent/work/.ui/journal.jsonl","utf8").trim().split("\n");
const seqs=lines.map(l=>JSON.parse(l).seq);
const ok=seqs.every((s,i)=>s===i+1);
console.log(ok?"连续":"断链: "+seqs.join(","));
')
echo "seq: $SEQ_OK"
