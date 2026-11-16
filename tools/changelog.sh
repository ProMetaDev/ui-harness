#!/usr/bin/env bash
# 从 git 历史生成 CHANGELOG.md：按标签分段，按类型分组。
#
# 为什么不用工具：零依赖是这项目的约束，一个 40 行的脚本够了。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

OUT=${1:-CHANGELOG.md}

# 标签按版本排序（v0.1.0 → v0.1.10 要按数字比，不能按字典序）
mapfile -t TAGS < <(git tag -l 'v*' --sort=version:refname)
if [ "${#TAGS[@]}" -eq 0 ]; then
  echo "没有 v* 标签" >&2
  exit 1
fi

cat > "$OUT" <<'HEADER'
# 变更记录

由 `bash tools/changelog.sh` 从 git 历史生成，不要手改。
每条按类型分组：新功能 / 修复 / 工程 / 文档。

HEADER

# 从旧到新渲染；每段 = 上一个标签..这个标签
for i in "${!TAGS[@]}"; do
  tag="${TAGS[$i]}"
  if [ "$i" -eq 0 ]; then
    range="$tag"
    label="$tag（首个版本）"
  else
    range="${TAGS[$((i-1))]}..$tag"
    label="$tag"
  fi

  date=$(git log -1 --format=%ad --date=short "$tag")
  echo "## $label · $date" >> "$OUT"
  echo >> "$OUT"

  # 一行一条提交，去掉 Conventional Commits 前缀后分组
  git log --no-merges --format='%s' $range | while IFS= read -r subject; do
    [ -z "$subject" ] && continue
    case "$subject" in
      feat:*)  section="新功能" ;;
      fix:*)   section="修复" ;;
      ci:*|build:*|chore:*|eng:*) section="工程" ;;
      docs:*)  section="文档" ;;
      release:*) section="发布" ;;
      *)       section="其他" ;;
    esac
    body=$(echo "$subject" | sed -E 's/^[a-z]+(\([^)]*\))?:[[:space:]]*//')
    printf '%s\t%s\n' "$section" "$body"
  done | awk -F'\t' '
    {
      if (!seen[$1]++) { order[++n] = $1 }
      items[$1] = items[$1] "- " $2 "\n"
    }
    END {
      split("新功能 修复 工程 文档 发布 其他", prefer, " ")
      for (i = 1; i <= 6; i++) {
        s = prefer[i]
        if (s in items) { printf "**%s**\n\n%s\n", s, items[s] }
      }
    }
  ' >> "$OUT"
done

echo "已生成 $OUT（$(grep -c '^## ' "$OUT") 个版本段）"
