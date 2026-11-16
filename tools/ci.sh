#!/usr/bin/env bash
# 本地 CI 等价物：一条命令按顺序跑，任何一步失败就停。
#
#   bash tools/ci.sh            # 完整
#   bash tools/ci.sh --fast     # 跳过门禁（只做 preflight + test + release 打包检查）
#
# 在 GitHub Actions 上跑时，把 UI_BUILD_DIR 设成 checkout 目录即可（sync 会自动跳过）。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

FAST=0
[ "${1:-}" = "--fast" ] && FAST=1

step() { printf '\n===== %s =====\n' "$1"; }
fail() { printf '\nCI 失败：%s\n' "$1"; exit 1; }

step "1/5 前置检查"
bash tools/preflight.sh || fail "preflight"
grep -q '依赖齐全' <(bash tools/preflight.sh) || echo "（注意：有依赖缺失，部分检查会跳过）"

step "2/5 构建"
bash tools/dev.sh build --release || fail "构建"

step "3/5 版本与密钥"
bash tools/verify_version.sh || fail "版本接口"
bash tools/scan_secrets.sh || fail "密钥扫描"

step "4/5 测试"
bash tools/test.sh || fail "测试"

if [ "$FAST" -eq 0 ]; then
  step "5/5 门禁"
  bash tools/debug.sh || fail "门禁"
else
  step "5/5 门禁已跳过（--fast）"
fi

printf '\nCI 通过：%s\n' "$("$UI_BIN" --version)"
