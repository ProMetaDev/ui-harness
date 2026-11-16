#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
"$HOME/ui/target/release/ui" 2>&1 | head -32
echo "---- 退出码: $? ----"
