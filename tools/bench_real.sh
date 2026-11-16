#!/usr/bin/env bash
# 真模型对照：让 agent 自己跑一次实验（不是剧本），再打包、校验、出表。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
WS=${WS:-/tmp/mlbench-real/lr-s1}
rm -rf "$WS"; mkdir -p "$WS"
cp fixtures/ml/lr/train.py "$WS/"

# sh -c ??? nvm ? PATH,?????
NODE_BIN=$(command -v node || echo /home/fairy/.nvm/versions/node/v22.23.3/bin/node)
export UI_MODEL_CMD="$NODE_BIN /mnt/x/dsh_ws/harness-v2/tools/deepseek.mjs"

echo "=== 真模型跑 lr 实验（seed=1）==="
start=$(date +%s)
"$BIN" run \
  --task "工作区里有 train.py。请运行一次实验：python3 train.py --seed 1，然后告诉我准确率。结果文件是 metrics.json。" \
  --workspace "$WS" --mode standard --yes 2>&1 | tail -12
code=$?
echo "退出码 $code · 用时 $(( $(date +%s) - start ))s"

echo
echo "=== 打包 + 校验 ==="
"$BIN" lab pack --workspace "$WS" --name lr-s1-real --out /tmp/mlbench-real/bundle | tail -4
"$BIN" lab verify /tmp/mlbench-real/bundle; echo "verify 退出码: $?"

echo
echo "=== 报告摘录 ==="
sed -n '/## 产物/,/## 怎么验证/p' /tmp/mlbench-real/bundle/report.md
