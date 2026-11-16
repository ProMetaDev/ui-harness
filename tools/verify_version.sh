#!/usr/bin/env bash
# 版本接口：--version / -V / version 三者一致，且带 commit 与构建日期。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

a=$("$UI_BIN" --version)
b=$("$UI_BIN" -V)
c=$("$UI_BIN" version)

printf '%s' "$a" | grep -qE '^ui [0-9]+\.[0-9]+\.[0-9]+ \(commit [^,]+, built [^)]+\)$' || {
  echo "FAIL 格式不对: $a"; exit 1; }
[ "$a" = "$b" ] || { echo "FAIL -V 与 --version 不一致: $b"; exit 1; }
[ "$a" = "$c" ] || { echo "FAIL version 子命令不一致: $c"; exit 1; }
echo "PASS 版本接口一致: $a"
