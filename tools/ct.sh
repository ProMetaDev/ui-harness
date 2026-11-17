#!/usr/bin/env bash
# 在构建副本里跑 cargo，环境一律从 env.sh 来（cargo 版本、target 目录都统一）。
#
#   bash tools/ct.sh test -p ui-core sha256
#   bash tools/ct.sh build --release
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
bash tools/sync.sh >/dev/null
cd "$UI_BUILD_DIR" || exit 1
exec cargo "$@" --offline
