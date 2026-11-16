#!/usr/bin/env bash
# 一次性收尾：让 tag / 提交 / 产物 / 版本串完全一致
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

rm -f tools/_align.sh tools/_dirtyfix.mjs tools/_final011.sh
git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'build: untracked scratch files no longer mark a build dirty' 2>/dev/null || echo "（无改动）"

bash tools/dev.sh build --release 2>&1 | tail -1
V=$("$UI_BIN" --version)
echo "版本: $V"

case "$V" in
  *-dirty*) echo "仍然是 dirty，停止"; exit 1 ;;
esac

git tag -f -a v0.1.1 -m 'ui 0.1.1: first build passing full CI' >/dev/null
git push origin main 2>&1 | tail -1
git push --force origin v0.1.1 2>&1 | tail -1

rm -rf dist
bash tools/release.sh 2>&1 | tail -3

echo "== 一致性 =="
echo "tag   : $(git rev-list -n1 v0.1.1 | cut -c1-7)"
echo "main  : $(git rev-parse --short HEAD)（远端 $(git ls-remote origin refs/heads/main | cut -c1-7)）"
echo "版本  : $V"
echo "工作区: $(git status --porcelain | wc -l) 个改动"
