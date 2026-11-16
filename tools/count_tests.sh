#!/usr/bin/env bash
set -uo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
bash tools/test.sh 2>&1 | grep -oE 'test result: ok\. [0-9]+ passed' | awk '{s+=$4} END {print "total tests:", s}'
