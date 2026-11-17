#!/usr/bin/env bash
# v1.0.0 发布前检查 + 发布。任何一步不过就停，绝不"跳过继续"。
#
#   bash tools/release_v1.sh            # 只检查
#   bash tools/release_v1.sh --tag      # 检查全过之后打 tag 并推送
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

DO_TAG=0
[ "${1:-}" = "--tag" ] && DO_TAG=1

fail() { echo "✗ $*" >&2; exit 1; }
step() { echo; echo "== $* =="; }

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
echo "版本: $VERSION"

step "1/6 工作区是否干净（除了别人的 UI 文件）"
dirty=$(git status --porcelain | grep -v '^?? ' | grep -vE 'ui-cli/assets/chat.html|ui-cli/src/server.rs|docs/03-frontend.md|tools/serve_smoke.sh' || true)
if [ -n "$dirty" ]; then
  echo "$dirty"
  fail "还有未提交的改动，先提交再发版"
fi
echo "✓ 没有未提交的内核改动"

step "2/6 全量测试"
bash tools/test.sh > /tmp/v1-test.log 2>&1 || { tail -30 /tmp/v1-test.log; fail "测试失败"; }
grep -E 'test result: ok' /tmp/v1-test.log | awk -F' ' '{s+=$4} END {print "✓ 通过用例: " s}'
grep -q '测试通过' /tmp/v1-test.log || fail "test.sh 没有输出「测试通过」"

step "3/6 门禁"
bash tools/debug.sh > /tmp/v1-gate.log 2>&1 || { grep -E 'FAIL' /tmp/v1-gate.log | head -10; fail "门禁失败"; }
line=$(grep -E '^== [0-9]+ passed' /tmp/v1-gate.log | tail -1)
echo "✓ $line"
echo "$line" | grep -q ', 0 failed' || fail "门禁有失败项"

step "4/6 干净检出的 HEAD 可构建（门禁已含，这里再独立确认一次）"
bash tools/verify_head_clean.sh > /tmp/v1-clean.log 2>&1 || { tail -20 /tmp/v1-clean.log; fail "干净检出构建失败"; }
tail -1 /tmp/v1-clean.log

step "5/6 发布产物"
bash tools/release.sh --out /tmp/ui-dist-v1 > /tmp/v1-release.log 2>&1 || { tail -20 /tmp/v1-release.log; fail "打包失败"; }
grep -E '包:|sha256:|自校验' /tmp/v1-release.log | tail -3

step "6/6 结论"
SHA=$(git rev-parse --short HEAD)
REMOTE=$(git ls-remote origin refs/heads/main | cut -c1-7)
echo "本地 HEAD: $SHA"
echo "远端 main: $REMOTE"
[ "$SHA" = "$REMOTE" ] || fail "本地与远端不一致，先推送"

if [ "$DO_TAG" = "1" ]; then
  bash tools/tag.sh "v$VERSION" "$SHA" "ui $VERSION"
else
  echo
  echo "全部检查通过。打 tag 请运行："
  echo "  bash tools/release_v1.sh --tag"
fi
