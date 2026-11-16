#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 快路径：同步到 WSL 副本，再在那里跑 cargo。
#
#   bash tools/dev.sh test
#   bash tools/dev.sh build --release
#   bash tools/dev.sh run --release
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
bash "$HERE/sync.sh"
cd "$UI_BUILD_DIR"

# 构建副本在 ext4 上，target 也用默认位置即可
exec cargo "$@" --offline
