#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
echo "== 版本 =="
"$UI_BIN" --version
echo "== 发布包自检 =="
bash tools/release.sh 2>&1 | tail -5
echo "== 提交并打 v0.1.1 =="
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'release: v0.1.1 (first green CI: journal multi-writer race fixed)' || echo "（无改动）"
git tag -a v0.1.1 -m 'ui 0.1.1: first build passing full CI' 2>/dev/null || echo "（tag 已存在）"
git push origin main 2>&1 | tail -2
git push origin v0.1.1 2>&1 | tail -2
echo "== 远端标签 =="
git ls-remote --tags origin | awk '{print $2}' | tr '\n' ' '
