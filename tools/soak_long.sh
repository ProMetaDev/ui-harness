#!/usr/bin/env bash
# 长跑入口：默认 24h，按 UI_SOAK_SECONDS 调短用于本地验收。
# 每轮 run + verify；失败立即退出；记录轮次、RSS、fd、耗时。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

BIN="$UI_BIN"
WS="${UI_SOAK_WS:-/tmp/ui-soak-long}"
SECONDS_TOTAL="${UI_SOAK_SECONDS:-86400}"
LOG="${UI_SOAK_LOG:-/tmp/ui-soak-long.log}"
rm -rf "$WS"
mkdir -p "$WS"
: > "$LOG"

start=$(date +%s)
deadline=$((start + SECONDS_TOTAL))
i=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  i=$((i + 1))
  t0=$(date +%s)
  "$BIN" run --workspace "$WS" --task "soak-long-$i" \
    --mock fixtures/mock.json --yes >>"$LOG" 2>&1
  "$BIN" verify --workspace "$WS" >>"$LOG" 2>&1
  grep -q '哈希链完整' "$LOG" || { echo "FAIL: journal 链损坏（轮次 $i）"; exit 1; }
  rss=$(ps -o rss= -p $$ 2>/dev/null | tr -d ' ' || echo 0)
  fds=$(ls /proc/$$/fd 2>/dev/null | wc -l || echo 0)
  printf 'round=%s elapsed=%ss rss=%sKB fds=%s\n' "$i" "$(( $(date +%s) - start ))" "$rss" "$fds" | tee -a "$LOG"
  sleep 1
done

if command -v pgrep >/dev/null 2>&1 && pgrep -f "ui-soak-long" >/dev/null 2>&1; then
  echo "FAIL: 仍有 ui-soak-long 进程" >&2
  pgrep -af "ui-soak-long" >&2 || true
  exit 1
fi
echo "PASS soak-long rounds=$i seconds=$SECONDS_TOTAL"
