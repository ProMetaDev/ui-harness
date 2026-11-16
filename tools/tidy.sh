#!/usr/bin/env bash
# 收尾：把散落的东西归位，清掉临时产物。先打印，再动手。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2

echo "=== 准备处理 ==="
ls -d -- 'C:Users31440' 2>/dev/null && echo "  ↑ 早先 \$HOME 展开失败留下的目录"
ls 01-diagnosis.md 2>/dev/null && echo "  ↑ 设计文档应该进 docs/"

echo
echo "=== 执行 ==="
[ -d 'C:Users31440' ] && rm -rf -- 'C:Users31440' && echo "removed C:Users31440"
[ -f 01-diagnosis.md ] && mv 01-diagnosis.md docs/01-diagnosis.md && echo "moved 01-diagnosis.md -> docs/"

rm -rf out/acceptance/probe && echo "removed out/acceptance/probe"
rm -f out/dom.txt out/dom.err out/say.json && echo "removed out 临时文件"
[ -d out/webd ] && rm -rf out/webd && echo "removed out/webd"

# 验收副本里的 target 是 cargo 在副本内编译的产物，删掉不影响日志
find out/acceptance -maxdepth 4 -type d -name target -prune -exec rm -rf {} + 2>/dev/null
echo "removed acceptance 副本里的 target/"

echo
echo "=== 之后 ==="
du -sh out 2>/dev/null
du -sh out/acceptance/* 2>/dev/null
ls out | tr '\n' ' '; echo
ls docs
