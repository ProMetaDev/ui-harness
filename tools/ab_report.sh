#!/usr/bin/env bash
set -uo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"/out/acceptance/ab
for a in a b; do
  echo "== arm $a =="
  grep 'tool/trim' "$a/repo/.ui/journal.jsonl" 2>/dev/null | sed -E 's/.*"tool":"([^"]*)".*"totalChars":([0-9]+),"keptChars":([0-9]+).*/\1 total=\2 kept=\3/' | head -5
  echo "-- 折叠事件 --"
  grep -c 'compact' "$a/repo/.ui/journal.jsonl" 2>/dev/null || true
done
