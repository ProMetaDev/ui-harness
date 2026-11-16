#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
cat > /tmp/adapter-in.json <<'EOF'
{"prompt":"回答两个字：收到","repair":null}
EOF
bash tools/_node.sh tools/deepseek.mjs < /tmp/adapter-in.json | head -3
