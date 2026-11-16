#!/usr/bin/env bash
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
bash tools/dev.sh build --release 2>&1 | tail -1
rm -rf out/acceptance/probe
bash tools/_node.sh tools/acceptance_prepare2.js out/acceptance/probe/repo >/dev/null
UI_MODEL_CMD='bash tools/_node.sh tools/deepseek.mjs' "$BIN" run \
  --task-file fixtures/acceptance-task2.txt \
  --workspace out/acceptance/probe/repo \
  --yes --max-calls 3 2>&1 | head -14
