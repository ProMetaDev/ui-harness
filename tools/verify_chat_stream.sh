#!/usr/bin/env bash
# 网页聊天流的离线校验：不需要浏览器/网络，用假 DOM + 假 EventSource 跑 chat.html 的脚本。
# 没有 node 的机器上跳过（本地 WSL 常常没有；CI 的 ubuntu runner 自带）。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

if ! command -v node >/dev/null 2>&1; then
  echo "SKIP 没有 node，跳过（CI 上会跑）"
  exit 0
fi

node tools/check_chat_stream.mjs
