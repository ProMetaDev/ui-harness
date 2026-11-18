#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
cp "$UI_BUILD_DIR/Cargo.lock" Cargo.lock
grep -A1 'name = "ui-' Cargo.lock | grep version | head -2
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q --amend --no-edit
echo "提交: $(git rev-parse --short HEAD)"
git status --porcelain
echo "文件数: $(git show --stat --oneline HEAD | wc -l)"
