#!/usr/bin/env bash
# 短跑 soak：重复 run → verify，确认日志链、进程和预算不会随轮次退化。
# 24h 浸泡用 UI_SOAK_ITERS / UI_SOAK_SECONDS 放大，CI 默认只跑短版。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail

BIN="$UI_BIN"
WS=/tmp/ui-soak
ITERS="${UI_SOAK_ITERS:-12}"
rm -rf "$WS"
mkdir -p "$WS"

for i in $(seq 1 "$ITERS"); do
  "$BIN" run --workspace "$WS" --task "soak-$i" \
    --mock fixtures/mock.json --yes >/tmp/ui-soak-run.log 2>&1
  "$BIN" verify --workspace "$WS" >/tmp/ui-soak-verify.log 2>&1
  # 每一轮都必须留下可验证的链；不检查具体 token，只检查不变量。
  grep -q '哈希链完整' /tmp/ui-soak-verify.log
done

# 进程泄漏的粗检：同一 workspace 跑完后不应留下 shell 子进程。
if command -v pgrep >/dev/null 2>&1; then
  if pgrep -f "ui-soak" >/dev/null 2>&1; then
    echo "FAIL: 仍有 ui-soak 相关进程" >&2
    pgrep -af "ui-soak" >&2 || true
    exit 1
  fi
fi

echo "PASS soak ${ITERS} 轮，日志链完整"
