#!/usr/bin/env bash
# 本地 CI 等价物（GitHub Actions 也直接调用它）。
#
#   bash tools/ci.sh            # 完整
#   bash tools/ci.sh --fast     # 跳过门禁（preflight + 构建 + 版本/密钥 + 测试）
#
# 两个设计要点（都是踩过的坑）：
# 1. 顺序不能错：版本检查依赖二进制，构建必须在它之前 —— 这个顺序错了让 CI 红过三次。
# 2. 失败时打印该步最后 40 行 —— 否则在 Actions 页面上只有一行 FAIL，无从下手。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

FAST=0
[ "${1:-}" = "--fast" ] && FAST=1

# 日志写固定路径：失败时 GitHub Actions 会把它当 artifact 带走（/tmp 随机名拿不到）
STEP_LOG="$UI_ROOT/.ci-step.log"
: > "$STEP_LOG"

step() { printf '\n===== %s =====\n' "$1"; }

run_step() {
  local name="$1"
  shift
  step "$name"
  if "$@" 2>&1 | tee "$STEP_LOG"; then
    return 0
  fi
  printf '\n---- %s 失败，最后 40 行 ----\n' "$name"
  tail -40 "$STEP_LOG"
  printf '\n[CI] 失败于：%s\n' "$name"
  exit 1
}

run_step "1/5 前置检查" bash tools/preflight.sh
run_step "2/5 构建" bash tools/dev.sh build --release
run_step "3/5 版本" bash tools/verify_version.sh
run_step "4/5 密钥扫描" bash tools/scan_secrets.sh
run_step "5/5 测试" bash tools/test.sh

if [ "$FAST" -eq 0 ]; then
  run_step "门禁" bash tools/debug.sh
else
  step "门禁已跳过（--fast）"
fi

rm -f "$STEP_LOG"
printf '\nCI 通过：%s\n' "$("$UI_BIN" --version)"
