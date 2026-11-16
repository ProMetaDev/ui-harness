#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
echo "BASH_ENV=[${BASH_ENV:-}] ENV=[${ENV:-}]"
echo "--- 直接跑 _node.sh 的 stdout ---"
bash tools/_node.sh -e 'console.log("hello")' 2>/tmp/n.err | od -c | head -3
echo "stderr: $(cat /tmp/n.err)"
echo "--- 用 sh -c 跑（和内核同样的方式）---"
sh -c "bash tools/_node.sh -e 'console.log(\"hello\")'" 2>/tmp/n2.err | od -c | head -3
echo "stderr: $(cat /tmp/n2.err)"
