#!/usr/bin/env bash
# 隔离副本基准（bash 包装：把当前二进制通过环境变量交给 python 计时器）
#   bash tools/bench_isolate.sh [基准二进制]
#
# 为什么用 python 计时：WSL2 的墙钟会被 Hyper-V 时间同步校正，偶发向后跳，
# 用 date 算差值会出现负耗时（真实踩到过 -835 ms）。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
UI_BIN="$UI_BIN" exec python3 tools/bench_isolate.py "${1:-}"
