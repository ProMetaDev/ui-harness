#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
WS=out/win-jobtest-c
rm -rf "$WS"; mkdir -p "$WS"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"detached child","toolCalls":[{"name":"shell","args":{"command":"cmd /c start /b ping -n 60 127.0.0.1","timeoutMs":6000}}]},
  {"content":"done"}]}
JSON
echo "场景 C 已写入（配合 UI_JOB_PROCS=1 使用）"
