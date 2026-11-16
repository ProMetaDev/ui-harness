#!/usr/bin/env bash
# 真模型实验链路：3 个 seed × 2 个任务，全走 ui 内核 + gpt-6-astra。
#
# 目的：验证「结论必须有产物背书」在真实模型下成立，而不是只在 mock 剧本里成立。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

NODE=$(command -v node || echo /home/fairy/.nvm/versions/node/v22.23.3/bin/node)
export UI_MODEL_CMD="$NODE $UI_ROOT/tools/chat_compat.mjs"

ROOT=/tmp/ui-lab-real
rm -rf "$ROOT"; mkdir -p "$ROOT"
BUNDLES=()
FAILED=0

for task in lr mlp; do
  for seed in 1 2 3; do
    ws="$ROOT/${task}-${seed}"
    mkdir -p "$ws/fixtures" "$ws/artifacts"
    cp -r fixtures/ml/* "$ws/fixtures/" 2>/dev/null || true

    echo
    echo "══════ ${task} / seed ${seed} ══════"
    start=$(date +%s)
    "$UI_BIN" run \
      --task "运行 python3 fixtures/${task}/train.py --seed ${seed} 训练模型；把 metrics.json 移到 artifacts/metrics.json；最后报告 accuracy 数值。" \
      --workspace "$ws" --yes --max-calls 8 2>&1 | tail -8
    code=${PIPESTATUS[0]}
    echo "  退出码 $code · $(( $(date +%s) - start ))s"

    # 打包：产物哈希 + 输入血统 + 环境指纹 + 日志链
    if ! "$UI_BIN" lab pack --name "${task}-s${seed}" --workspace "$ws" >/dev/null 2>&1; then
      echo "  lab pack 失败"
      FAILED=$((FAILED+1))
      continue
    fi
    bundle="$ws/experiments/${task}-s${seed}"
    BUNDLES+=("$bundle")

    # 从真实产物里读出 accuracy，再声明结论（声明必须被产物背书）
    acc=$(python3 -c "
import json,sys
try:
    d=json.load(open('$ws/artifacts/metrics.json'))
    print(d['accuracy'])
except Exception:
    print('')
" 2>/dev/null)
    if [ -n "$acc" ]; then
      echo "  产物 accuracy = $acc"
      "$UI_BIN" lab claim --name "accuracy=${task}" --value "$acc" --workspace "$ws" >/dev/null 2>&1
    else
      echo "  产物里没有 accuracy（模型可能没把 metrics.json 放到 artifacts/）"
    fi
  done
done

echo
echo "══════ 校验每个实验包 ══════"
for b in "${BUNDLES[@]}"; do
  if "$UI_BIN" lab verify "$b" >/dev/null 2>&1; then
    echo "PASS $(basename "$b")"
  else
    echo "FAIL $(basename "$b")"
    "$UI_BIN" lab verify "$b" 2>&1 | tail -6
    FAILED=$((FAILED+1))
  fi
done

echo
echo "══════ 多 seed 对照表 ══════"
"$UI_BIN" lab table "${BUNDLES[@]}" 2>&1 | tail -20

echo
echo "实验包: $ROOT/*/experiments/"
echo "失败项: $FAILED"
[ "$FAILED" -eq 0 ] || exit 1
