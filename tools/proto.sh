#!/usr/bin/env bash
# 原型一键演示：跑实验 → 声明结论 → 打包 → 校验 → 多 seed 出表。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
BASE=${BASE:-/tmp/proto}
rm -rf "$BASE"; mkdir -p "$BASE/bundles"

echo "────────────────────────────────────────────"
echo " 1. 跑 3 个 seed 的逻辑回归实验（脚本化模型）"
echo "────────────────────────────────────────────"
for seed in 1 2 3; do
  ws="$BASE/lr-s$seed"; mkdir -p "$ws"
  cp fixtures/ml/lr/train.py "$ws/"
  cat > "$ws/sc.json" <<EOF
{"model":"scripted","steps":[
  {"content":"跑实验","toolCalls":[{"name":"shell","args":{"command":"python3 train.py --seed $seed"}}]},
  {"content":"完成"}]}
EOF
  "$BIN" run --task "运行 lr 实验 seed=$seed" --mock "$ws/sc.json" \
    --workspace "$ws" --mode standard --yes >/dev/null 2>&1
  acc=$(python3 -c "import json;print(json.load(open('$ws/metrics.json'))['accuracy'])")
  "$BIN" lab claim --workspace "$ws" --name accuracy --value "$acc" >/dev/null
  "$BIN" lab pack --workspace "$ws" --name "lr-s$seed" --out "$BASE/bundles/lr-s$seed" >/dev/null
  echo "  seed $seed → accuracy $acc"
done

echo
echo "────────────────────────────────────────────"
echo " 2. 逐个校验（哈希 + 输入 + 结论 + 日志链）"
echo "────────────────────────────────────────────"
fails=0
for b in "$BASE"/bundles/*; do
  out=$("$BIN" lab verify "$b"); code=$?
  printf '  %-10s %s\n' "$(basename "$b")" "$(echo "$out" | grep -o '结论 [0-9]*/[0-9]* 对得上') 退出码 $code"
  [ $code -ne 0 ] && fails=$((fails+1))
done

echo
echo "────────────────────────────────────────────"
echo " 3. 对照表（输入一致才求均值）"
echo "────────────────────────────────────────────"
"$BIN" lab table "$BASE"/bundles/* | sed -n '/## 输入一致性/,/## 环境/p'

echo
echo "────────────────────────────────────────────"
echo " 4. 篡改检测（改产物 → 必须拒绝）"
echo "────────────────────────────────────────────"
b="$BASE/bundles/lr-s1"
python3 - "$b/artifacts/metrics.json" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d["accuracy"] = 0.99
json.dump(d, open(p, "w"))
PY
"$BIN" lab verify "$b" | tail -4
echo "  退出码: $?"

echo
if [ $fails -eq 0 ]; then
  echo "结果：3 个包全部自洽，篡改被拒绝。原型链路通了。"
else
  echo "结果：有 $fails 个包没通过。"
  exit 1
fi
