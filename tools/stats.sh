#!/usr/bin/env bash
set -uo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
echo "--- 代码量 ---"
find ui-core ui-cli -name '*.rs' | xargs wc -l | tail -1
echo "--- 内核模块 ---"
ls ui-core/src/*.rs | xargs -n1 basename | tr '\n' ' '
echo
echo "--- 工具 ---"
grep -oE 'name: "[a-z_]+"' ui-core/src/tools.rs | sort -u | sed 's/name: //' | tr '\n' ' '
echo
echo "--- 测试 ---"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
cargo test --offline 2>/dev/null | grep -oE 'test result: ok\. [0-9]+ passed' | awk '{s+=$4} END {print s " 个"}'
echo "--- 文档 / 脚本 ---"
ls docs | tr '\n' ' '; echo
ls tools | wc -l
