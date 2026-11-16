#!/usr/bin/env bash
# 连跑三次网页能力探针，看稳不稳（门禁不该是抽奖）。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
for i in 1 2 3; do
  echo "--- run $i ---"
  bash tools/probe_web_features.sh "785$i" 2>&1 | grep -E 'FAIL|全部通过|有失败' || echo "(无输出)"
done
