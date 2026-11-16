#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
bash tools/dev.sh build --release 2>&1 | grep -E '^error|Finished' | head -3
"$UI_BIN" --version
bash tools/preflight.sh | tail -3
