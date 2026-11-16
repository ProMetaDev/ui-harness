#!/usr/bin/env bash
set -uo pipefail
echo "== git =="
git --version 2>&1 || echo "没有 git"
git config --global user.name 2>/dev/null || echo "(global user.name 未设)"
git config --global user.email 2>/dev/null || echo "(global user.email 未设)"
echo
echo "== 磁盘 =="
df -h /mnt/x /home 2>/dev/null | sed 's/  */ /g'
echo
echo "== Windows 盘符（/mnt/*) =="
ls -d /mnt/*/ 2>/dev/null | tr '\n' ' '
echo
echo "== 仓库大小分布 =="
cd /mnt/x/dsh_ws/harness-v2
du -sh out docs fixtures ui-core ui-cli tools 2>/dev/null
