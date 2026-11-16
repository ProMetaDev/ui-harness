#!/usr/bin/env bash
# 前置检查：说清楚"这台机器上哪些门禁项会跳过"，而不是静默跳过。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"

ok()   { printf '  %-10s %-6s %s\n' "$1" "$2" "$3"; }

missing=0
check() {
  local name="$1" need="$2" why="$3"
  if eval "$need" >/dev/null 2>&1; then
    ok "$name" "有" "—"
  else
    ok "$name" "缺" "$why"
    missing=$((missing + 1))
  fi
}

echo "前置检查（缺什么会明说，不静默跳过）"
check "cargo"    "command -v cargo"        "无法构建"
check "git"      "command -v git"          "版本号会显示 unknown"
check "python3"  "command -v python3"      "独立日志校验器、验证包、ML 基准会跳过"
check "curl"     "command -v curl"         "所有 HTTP 冒烟测试会跳过"
if command -v node >/dev/null 2>&1; then
  ok "node" "有" "—"
else
  ok "node" "缺" "只影响真模型适配器（tools/deepseek.mjs），不影响门禁"
fi
check "rsync"    "command -v rsync"        "sync 会退化成 cp（更慢但不影响正确性）"

# sklearn 只影响 ML 基准
if python3 -c "import sklearn" >/dev/null 2>&1; then
  ok "sklearn" "有" "—"
else
  ok "sklearn" "缺" "ML 基准（3 任务 × 3 seed）会跳过"
  missing=$((missing + 1))
fi

# mingw 只影响 Windows 交叉编译
MINGW="$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix"
if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 || [ -x "$MINGW" ]; then
  ok "mingw" "有" "—"
else
  ok "mingw" "缺" "Windows 交叉编译不可用（bash tools/bootstrap.sh 可装）"
  missing=$((missing + 1))
fi

echo
if [ "$missing" -eq 0 ]; then
  GATE=$(grep -c '^run ' "$UI_ROOT/tools/debug.sh")
echo "结论：依赖齐全，$GATE 项门禁可以全跑。"
  exit 0
fi
echo "结论：缺 $missing 项依赖，部分检查会跳过（不会假绿，但覆盖不完整）。"
echo "补依赖：bash tools/bootstrap.sh"
exit 0
