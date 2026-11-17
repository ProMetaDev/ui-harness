#!/usr/bin/env bash
# 按提交 SHA 打 tag，而不是按 HEAD。
#
# 为什么：仓库可能有并行工作（比如别人同时在提交 UI）。`git tag HEAD`
# 会把 tag 打到别人刚提的、还没跑过 CI 的提交上——版本号就不可信了。
#
# 用法：
#   bash tools/tag.sh v0.1.12 <sha> "说明"
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

TAG=${1:-}
SHA=${2:-}
MSG=${3:-}

if [ -z "$TAG" ] || [ -z "$SHA" ]; then
  sed -n '2,9p' "$0"
  exit 1
fi

FULL=$(git rev-parse --verify "$SHA^{commit}" 2>/dev/null) || {
  echo "找不到提交: $SHA" >&2
  exit 1
}
SHORT=$(git rev-parse --short "$FULL")
REMOTE=$(git ls-remote origin refs/heads/main | cut -f1)

if [ "$FULL" != "$REMOTE" ]; then
  echo "拒绝打 tag：$SHORT 不是远端 main（远端 $(echo "$REMOTE" | cut -c1-7)）。" >&2
  echo "tag 必须指向**已经推送且跑过 CI**的提交。" >&2
  exit 1
fi

echo "普通检查通过：$SHORT == 远端 main"
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "tag $TAG 已存在：$(git rev-list -n1 "$TAG" | cut -c1-7)"
  read -r -p "覆盖？[y/N] " ans
  [ "$ans" = "y" ] || exit 1
  git tag -d "$TAG" >/dev/null
fi

git tag -a "$TAG" "$FULL" -m "${MSG:-$TAG}"
git push origin "$TAG" 2>&1 | tail -1
echo "已打 $TAG → $SHORT"
