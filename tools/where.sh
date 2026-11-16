#!/usr/bin/env bash
set -uo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
echo "=== 顶层 ==="
ls -1
echo
echo "=== out/ ==="
du -sh out 2>/dev/null
ls -1 out | head -20
echo
echo "=== out/acceptance ==="
find out/acceptance -maxdepth 2 -type d | head -12
echo
echo "=== 日志与运行痕迹（demo 里）==="
du -sh demo/.ui 2>/dev/null; ls demo/.ui 2>/dev/null
echo
echo "=== 文档 / 夹具 / 脚本 ==="
ls docs; echo "--"; ls fixtures; echo "--"; ls tools | wc -l
echo
echo "=== WSL 里的构建副本 ==="
du -sh "$UI_BUILD_DIR"" "$UI_TARGET_DIR"" 2>/dev/null
echo
echo "=== 密钥（只看是否存在） ==="
ls -la secrets 2>/dev/null | sed 's/ [a-z]*$//' | head -5
