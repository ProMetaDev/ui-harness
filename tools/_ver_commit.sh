#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# 锁文件跟上 0.2.0
bash tools/ct.sh build --release >/dev/null 2>&1
cp "$UI_BUILD_DIR/Cargo.lock" Cargo.lock
grep -A1 'name = "ui-' Cargo.lock | grep version | head -2

bash tools/changelog.sh >/dev/null 2>&1 || true

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'release: withdraw v1.0.x, adopt 0.x semver with a verified 1.0 gate (docs/34)'
echo "提交: $(git rev-parse --short HEAD)"
git push origin main 2>&1 | tail -1
"$UI_BIN" --version
