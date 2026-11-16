#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
echo "== 可能含密钥的路径 =="
find . -maxdepth 3 \( -name 'secrets' -o -name 'providers.toml' -o -name '*.key' -o -name '.env*' \) 2>/dev/null | head -20
echo
echo "== 顶层内容 =="
ls -A | tr '\n' ' '
echo
echo "== 大文件（>1MB，前 10）=="
find . -type f -size +1M 2>/dev/null | head -10 | while read -r f; do du -h "$f"; done
