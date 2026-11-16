#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
echo "== 版本控制 =="
if [ -d .git ]; then
  git status --porcelain | head -5
  echo "提交数: $(git rev-list --count HEAD 2>/dev/null || echo 0)"
else
  echo "没有 .git（无版本控制）"
fi
echo
echo "== 依赖 =="
grep -c 'dependencies' Cargo.toml 2>/dev/null || true
cat Cargo.toml
echo
echo "== 备份 =="
ls -d /mnt/x/dsh_ws/harness-v2.* 2>/dev/null || echo "没有备份副本"
