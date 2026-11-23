#!/usr/bin/env bash
# 低频 flake 的稳定性循环：默认 20 次 lab_pack 回归。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

N="${UI_FLAKE_ITERS:-20}"
for i in $(seq 1 "$N"); do
  if ! cargo test -p ui-cli --test cli lab_pack_keeps_artifacts_flat_and_not_as_inputs \
      --quiet >"/tmp/ui-flake-$i.log" 2>&1; then
    echo "FAIL iteration $i" >&2
    tail -40 "/tmp/ui-flake-$i.log" >&2
    exit 1
  fi
  printf 'ok %s/%s\n' "$i" "$N"
done
echo "PASS flake loop $N iterations"
