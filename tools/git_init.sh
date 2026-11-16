#!/usr/bin/env bash
set -euo pipefail
cd /mnt/x/dsh_ws/harness-v2

if [ -d .git ]; then
  echo "已经有 .git，跳过初始化"
else
  git init -q -b main
  echo "已初始化仓库（分支 main）"
fi

git add -A

echo "== 将要提交的文件数 =="
git diff --cached --name-only | wc -l

echo "== 安全检查：暂存区里有没有密钥路径 =="
if git diff --cached --name-only | grep -Ei 'secrets/|providers\.toml|\.env|\.key$'; then
  echo "!! 发现疑似密钥文件，中止提交"
  exit 1
fi
echo "干净"

echo "== 暂存区构成（按目录）=="
git diff --cached --name-only | sed 's|/.*||' | sort | uniq -c | sort -rn

echo "== 体积 =="
git diff --cached --stat | tail -1
