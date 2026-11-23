#!/usr/bin/env bash
# 干净安装验证：空 CARGO_TARGET_DIR + 当前源码构建，再跑 --version/doctor。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

TARGET=$(mktemp -d /tmp/ui-clean-target.XXXXXX)
WS=$(mktemp -d /tmp/ui-clean-ws.XXXXXX)
trap 'rm -rf "$TARGET" "$WS"' EXIT

echo "clean build target: $TARGET"
CARGO_TARGET_DIR="$TARGET" cargo build --release -p ui-cli --quiet
BIN="$TARGET/release/ui"
test -x "$BIN" || { echo "FAIL: 干净构建没有产出 ui"; exit 1; }
"$BIN" --version
"$BIN" doctor --workspace "$WS" >/tmp/ui-clean-doctor.log 2>&1 || {
  echo "FAIL: 干净构建的 ui doctor 失败"; tail -40 /tmp/ui-clean-doctor.log; exit 1;
}
grep -q "workspace" /tmp/ui-clean-doctor.log || { echo "FAIL: doctor 没有 workspace 输出"; exit 1; }
echo "PASS clean build + version + doctor"
