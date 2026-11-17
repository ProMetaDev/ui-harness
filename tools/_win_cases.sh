#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# 场景 A：普通子进程（ping 是 cmd 的子进程）
WS=out/win-jobtest-a
rm -rf "$WS"; mkdir -p "$WS"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"long child","toolCalls":[{"name":"shell","args":{"command":"ping -n 60 127.0.0.1","timeoutMs":6000}}]},
  {"content":"done"}]}
JSON

# 场景 B：start /b 脱离的子进程（不用 Job 的话很容易漏杀）
WS=out/win-jobtest-b
rm -rf "$WS"; mkdir -p "$WS"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"detached child","toolCalls":[{"name":"shell","args":{"command":"cmd /c start /b ping -n 60 127.0.0.1","timeoutMs":6000}}]},
  {"content":"done"}]}
JSON
echo "两个场景已写入"
