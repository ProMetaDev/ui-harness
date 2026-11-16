#!/usr/bin/env bash
# Deterministic showcase: reset the demo file, run one turn, replay the journal.
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export CARGO_TARGET_DIR="$HOME/ui-target"

cargo build --release --offline 2>&1 | tail -1

cat > demo/greet.js <<'EOF'
export function greet(name) {
  return `hi ${name}`;
}

console.log(greet("ui"));
console.log(greet(""));
EOF
rm -rf demo/.ui

echo "########## ui doctor ##########"
"$CARGO_TARGET_DIR/release/ui" doctor --workspace demo

echo
echo "########## before ##########"
grep -n 'return `hi' demo/greet.js

echo
echo "########## ui run ##########"
"$CARGO_TARGET_DIR/release/ui" run --task-file demo/task.txt \
  --mock fixtures/scenarios/fix-bug.json --workspace demo --yes

echo
echo "########## after ##########"
grep -n 'return `hi' demo/greet.js

echo
echo "########## ui replay ##########"
"$CARGO_TARGET_DIR/release/ui" replay --workspace demo
