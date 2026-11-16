#!/usr/bin/env bash
# 找到 node：优先 PATH，其次 nvm/常见位置。非登录 shell 里 PATH 常常没有 node。
set -uo pipefail
if command -v node >/dev/null 2>&1; then exec node "$@"; fi
for c in "$HOME"/.nvm/versions/node/*/bin/node /usr/local/bin/node /usr/bin/node /opt/node/bin/node; do
  if [ -x "$c" ]; then exec "$c" "$@"; fi
done
echo "找不到 node（用 bash tools/_node.sh 之外的路径调用，或先 source 你的 shell 配置）" >&2
exit 127
