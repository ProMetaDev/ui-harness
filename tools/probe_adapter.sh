#!/usr/bin/env bash
set -uo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
cat > /tmp/adapter-in.json <<'EOF'
{"prompt":"回答两个字：收到","repair":null}
EOF
bash tools/_node.sh tools/deepseek.mjs < /tmp/adapter-in.json | head -3
