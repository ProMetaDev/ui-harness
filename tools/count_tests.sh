#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
bash tools/test.sh 2>&1 | grep -oE 'test result: ok\. [0-9]+ passed' | awk '{s+=$4} END {print "total tests:", s}'
