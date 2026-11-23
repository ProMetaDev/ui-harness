#!/usr/bin/env bash
# 长跑入口：默认 24h，按 UI_SOAK_SECONDS 调短用于本地验收。
# 每轮 run + verify；失败立即退出；记录轮次、RSS、fd、耗时。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

BIN="$UI_BIN"
WS="${UI_SOAK_WS:-/tmp/ui-soak-long}"
SECONDS_TOTAL="${UI_SOAK_SECONDS:-86400}"
INTERVAL="${UI_SOAK_INTERVAL:-1}"
LOG="${UI_SOAK_LOG:-/tmp/ui-soak-long.log}"
STATE="${UI_SOAK_STATE:-/tmp/ui-soak-long-state.json}"
rm -rf "$WS"
mkdir -p "$WS"
: > "$LOG"
start=$(date +%s)
deadline=$((start + SECONDS_TOTAL))
i=0

state() {
  local phase="$1"
  printf '{"phase":"%s","rounds":%s,"startedAt":%s,"updatedAt":%s,"seconds":%s,"interval":%s}\n' \
    "$phase" "$i" "$start" "$(date +%s)" "$SECONDS_TOTAL" "$INTERVAL" > "$STATE.tmp"
  mv "$STATE.tmp" "$STATE"
}
state running

while [ "$(date +%s)" -lt "$deadline" ]; do
  i=$((i + 1))
  if ! "$BIN" run --workspace "$WS" --task "soak-long-$i" \
      --mock fixtures/mock.json --yes >>"$LOG" 2>&1; then
    state failed
    echo "FAIL: ui run 失败（轮次 $i）" >&2
    exit 1
  fi
  if ! "$BIN" verify --workspace "$WS" >>"$LOG" 2>&1; then
    state failed
    echo "FAIL: ui verify 失败（轮次 $i）" >&2
    exit 1
  fi
  if ! grep -q '哈希链完整' "$LOG"; then
    state failed
    echo "FAIL: journal 链损坏（轮次 $i）" >&2
    exit 1
  fi
  rss=$(ps -o rss= -p $$ 2>/dev/null | tr -d ' ' || echo 0)
  fds=$(ls /proc/$$/fd 2>/dev/null | wc -l || echo 0)
  printf 'round=%s elapsed=%ss rss=%sKB fds=%s\n' "$i" "$(( $(date +%s) - start ))" "$rss" "$fds" | tee -a "$LOG"
  state running
  sleep "$INTERVAL"
done

# 只找真正还在跑 ui 的进程；不要匹配父 shell 命令行里的路径字符串。
if command -v pgrep >/dev/null 2>&1 && pgrep -f "ui run.*ui-soak-long" >/dev/null 2>&1; then
  state failed
  echo "FAIL: 仍有 ui run --workspace ui-soak-long 进程" >&2
  pgrep -af "ui run.*ui-soak-long" >&2 || true
  exit 1
fi
state passed
echo "PASS soak-long rounds=$i seconds=$SECONDS_TOTAL state=$STATE"
