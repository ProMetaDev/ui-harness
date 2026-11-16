#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 并发写日志的稳定性：连跑 10 次，必须每次都链完整。
set -uo pipefail
cd "$UI_ROOT"
pass=0
for i in $(seq 1 10); do
  out=$(bash tools/verify_concurrent.sh 2>&1)
  if echo "$out" | grep -q '链完整' && echo "$out" | grep -q 'seq: 连续'; then
    pass=$((pass + 1))
  else
    echo "run $i FAILED:"
    echo "$out" | tail -6
  fi
done
echo "== $pass/10 通过"
[ "$pass" -eq 10 ]
