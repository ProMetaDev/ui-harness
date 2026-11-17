#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
for d in out/win-jobtest-a out/win-jobtest-b; do
  echo "== $d =="
  python3 - "$d/.ui/journal.jsonl" <<'PY'
import json, sys, os
p = sys.argv[1]
if not os.path.exists(p):
    print("  （没有日志）"); raise SystemExit
for line in open(p, encoding='utf-8'):
    e = json.loads(line)
    if e.get('kind') == 'tool/end' and e.get('tool') == 'shell':
        print('  sandbox meta:', (e.get('meta') or {}).get('sandbox'))
    if e.get('kind') == 'effect/observed':
        print('  observed   :', e.get('summary'))
PY
  echo
done
