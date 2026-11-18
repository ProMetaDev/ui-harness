#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# 锁文件跟上 0.2.1
bash tools/ct.sh build --release >/dev/null 2>&1
cp "$UI_BUILD_DIR/Cargo.lock" Cargo.lock
grep -A1 'name = "ui-' Cargo.lock | grep version | head -2

bash tools/changelog.sh >/dev/null 2>&1 || true

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'web: chat stream offline checks, snapshot approval cleanup, stricter SSE ordering test; 0.2.1'
echo "提交: $(git rev-parse --short HEAD)"
git show --stat --oneline HEAD | tail -8
git push origin main 2>&1 | tail -1
"$UI_BIN" --version
