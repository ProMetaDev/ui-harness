#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# B 组基准：3 个 CPU 任务 × 3 个 seed。每个 run 打包，最后每个任务出一张对照表。
# 默认用脚本化模型（零成本、可进 CI）；真模型对照见 REAL=1。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
BASE=${BASE:-/tmp/mlbench}
TASKS=${TASKS:-"lr mlp ablation"}
SEEDS=${SEEDS:-"1 2 3"}
REAL=${REAL:-0}

# ?? sklearn ???(????? python ???????)
if ! python3 -c "import sklearn" 2>/dev/null; then
  echo "??:?? sklearn"
  exit 0
fi

rm -rf "$BASE"; mkdir -p "$BASE/bundles" "$BASE/out"

for task in $TASKS; do
  for seed in $SEEDS; do
    ws="$BASE/$task-s$seed"
    mkdir -p "$ws"
    cp fixtures/ml/"$task"/train.py "$ws/"

    cat > "$ws/sc.json" <<EOF
{"model":"scripted","steps":[
  {"content":"跑实验（seed $seed）","toolCalls":[
    {"name":"shell","args":{"command":"python3 train.py --seed $seed"}}]},
  {"content":"实验完成，产物是 metrics.json"}]}
EOF

    "$BIN" run --task "运行 $task 实验，seed=$seed，结果写进 metrics.json" \
      --mock "$ws/sc.json" --workspace "$ws" --mode standard --yes \
      > "$BASE/out/$task-s$seed.log" 2>&1
    code=$?
    "$BIN" lab pack --workspace "$ws" --name "$task-s$seed" \
      --out "$BASE/bundles/$task-s$seed" > "$BASE/out/$task-s$seed.pack.log" 2>&1
    printf '%-10s seed %s  run=%s  %s\n' "$task" "$seed" "$code" \
      "$(grep -o '产物 [0-9]* 个' "$BASE/out/$task-s$seed.pack.log" || echo '打包失败')"
  done

  echo
  echo "===== $task ====="
  "$BIN" lab table $BASE/bundles/$task-s* > "$BASE/out/$task.table.md" 2>&1
  sed -n '/## 指标/,/## 环境/p' "$BASE/out/$task.table.md" | head -12
done

echo
echo "结果在 $BASE/out/*.table.md"
