#!/usr/bin/env bash
# 结论对账：声明的数字必须能在产物里找到；产物对不上就必须失败。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
WS=/tmp/ui-claim
B=/tmp/ui-claim-bundle
rm -rf "$WS" "$B"; mkdir -p "$WS"

cat > "$WS/run_exp.sh" <<'EOF'
printf '{"task":"lr","seed":1,"accuracy":0.72}\n' > metrics.json
EOF

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"跑实验","toolCalls":[{"name":"shell","args":{"command":"sh run_exp.sh"}}]},
  {"content":"准确率 0.72"}]}
EOF

echo "=== 跑一轮（实验在 run 内产出 metrics.json）+ 声明结论 ==="
"$BIN" run --task "跑实验并报告准确率" --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1 | grep -E '→|结束原因'
"$BIN" lab claim --workspace "$WS" --name accuracy --value 0.72

echo
echo "=== 打包（结论跟着包走，claims.json 不算产物）==="
"$BIN" lab pack --workspace "$WS" --name c1 --out "$B" | head -3
sed -n '/声明的结论/,/^$/p' "$B/report.md"

echo "=== 校验：结论对得上（应通过）==="
"$BIN" lab verify "$B"; echo "退出码: $?"

echo
echo "=== 篡改产物为 0.99，结论仍是 0.72（应失败）==="
printf '{"task":"lr","seed":1,"accuracy":0.99}\n' > "$B/artifacts/metrics.json"
"$BIN" lab verify "$B"; echo "退出码: $?"
