#!/usr/bin/env bash
set -euo pipefail
cd /mnt/x/dsh_ws/harness-v2
rm -f tools/env_check.sh tools/disk_check.sh tools/secrets_check.sh tools/repo_check.sh tools/help.sh
git add -A
echo "== 本次新增/修改 =="
git status --short | head -20
git commit -q -m "工具: 仓库初始化、备份（双盘镜像）、文档 12-repo"
echo "提交: $(git rev-parse --short HEAD) · 共 $(git rev-list --count HEAD) 个"
git log --oneline | head -3
