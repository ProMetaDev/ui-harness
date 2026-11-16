#!/usr/bin/env bash
# A3：--append-system-prompt 必须真的进到会发给模型的快照里
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
WS=/tmp/ui-sysprompt
rm -rf "$WS"; mkdir -p "$WS"

fail() { echo "FAIL: $1"; exit 1; }

full=$("$BIN" prompt --workspace "$WS" --append-system-prompt "SENTINEL-Q7：回答必须以 Q7 开头" --full)
echo "$full" | grep -q 'SENTINEL-Q7' || fail "追加指令没进快照"
echo "PASS 追加指令进了快照"

base=$("$BIN" prompt --workspace "$WS" --full)
echo "$base" | grep -q 'SENTINEL-Q7' && fail "不追加时不该出现" 
echo "PASS 不追加时不出现"

# 工具白名单也要反映在提示词里
t=$("$BIN" prompt --workspace "$WS" --no-tools --full)
echo "$t" | grep -q 'fs_write' && fail "--no-tools 时提示词仍列出 fs_write"
echo "PASS --no-tools 时提示词没有工具清单"

t2=$("$BIN" prompt --workspace "$WS" --tools fs_read --full)
echo "$t2" | grep -q 'fs_read' || fail "--tools fs_read 时提示词应有 fs_read"
echo "$t2" | grep -q 'fs_write' && fail "--tools fs_read 时提示词不该有 fs_write"
echo "PASS --tools 白名单反映在提示词里"
