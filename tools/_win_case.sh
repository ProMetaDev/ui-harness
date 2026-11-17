#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=out/win-jobtest
rm -rf "$WS"
mkdir -p "$WS"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"start a long command","toolCalls":[{"name":"shell","args":{"command":"ping -n 60 127.0.0.1","timeoutMs":1500}}]},
  {"content":"done"}]}
JSON
echo "sc.json 已写入 $WS"
cat "$WS/sc.json"
