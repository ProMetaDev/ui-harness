#!/usr/bin/env bash
# 构建 + 测试的固定入口（快路径：/mnt → ext4 副本再编译）。
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

echo "== sync + build =="
bash "$HERE/sync.sh"

cd "${UI_BUILD_DIR:-$HOME/ui}"

# 关键：cargo 的退出码必须成为脚本的退出码。
# 以前是 `cargo test | grep ... || true`，grep 的退出码把失败吞掉了——
# 门禁再去看输出才偶尔发现，于是变成「偶发红」。这里用临时文件留住退出码。
LOG=$(mktemp)
cargo test --offline >"$LOG" 2>&1
CODE=$?
grep -E "^error|error\[|^warning|test result|panicked|^test .* FAILED" "$LOG" || true
rm -f "$LOG"

if [ "$CODE" -ne 0 ]; then
  echo "== 测试失败（退出码 $CODE）=="
  exit "$CODE"
fi
echo "== 测试通过 =="
