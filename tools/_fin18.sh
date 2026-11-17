#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

rm -f tools/_poll.sh tools/_watch_ci.sh
git status --porcelain

git tag -a v0.1.8 -m 'ui 0.1.8: tail-only journal open (flat memory), streaming verify/replay, bench harness' 2>/dev/null || echo "（tag 已存在）"
git push origin v0.1.8 2>&1 | tail -1

bash tools/dev.sh build --release 2>&1 | tail -1
"$UI_BIN" --version
echo "标签: $(git ls-remote --tags origin | grep -v '\^{}' | awk -F/ '{print $3}' | tr '\n' ' ')"
echo "工作区: $(git status --porcelain | wc -l)"
