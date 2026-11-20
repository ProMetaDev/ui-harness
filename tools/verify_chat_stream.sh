#!/usr/bin/env bash
# 网页聊天流的离线校验：不需要浏览器/网络，用假 DOM + 假 EventSource 跑 chat.html 的脚本。
# 没有 node 的机器上跳过（本地 WSL 常常没有；CI 的 ubuntu runner 自带）。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

if ! command -v node >/dev/null 2>&1; then
  # 本地 WSL 常常没有 node；但 CI 上"静默跳过"等于这条检查不存在。
  # 所以 CI 用 UI_REQUIRE_NODE=1 跑，缺 node 直接失败。
  if [ "${UI_REQUIRE_NODE:-0}" = "1" ]; then
    echo "FAIL UI_REQUIRE_NODE=1 但找不到 node：这条检查在 CI 上不能跳过"
    exit 1
  fi
  echo "SKIP 没有 node，跳过（本地常见；CI 用 UI_REQUIRE_NODE=1 强制跑）"
  exit 0
fi

node tools/check_chat_stream.mjs
