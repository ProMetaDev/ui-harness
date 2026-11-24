#!/usr/bin/env bash
# 长跑入口：默认 24h，按 UI_SOAK_SECONDS 调短用于本地验收。
# 每轮 run + verify；失败/超时会明确记录后退出；state 用 tmp+mv 原子替换。
#
# 指标口径：只测被拉起的 ui 子进程（及其子进程树），绝不把本 shell 的 RSS 当 ui 指标。
#   rss：/usr/bin/time -v 可用时取 "Maximum resident set size"（权威，单位 KB）；
#        否则 spawn 后轮询 /proc/<pid>/status 的 VmRSS（进程树瞬时和取峰值）；
#        再无 /proc（macOS）则轮询 ps -o rss=。
#   fd ：/proc/<pid>/fd 计数（进程树求和取峰值）；无 /proc 时用 lsof；都没有就写 na。
#   指标来源写进每轮日志、$STATE（metrics/lastRound）和 $STATE.rounds.jsonl。
#   注：ui verify 常只有十几毫秒，短于采样窗口时 fd 会记 na（明确表示没采到，不伪造）；
#   ui run 的 RSS/fd 是验收主指标。
#
# 可用环境变量：
#   UI_SOAK_SECONDS        总时长秒（默认 86400）
#   UI_SOAK_INTERVAL       轮间隔秒（默认 1）
#   UI_SOAK_ROUND_TIMEOUT  单条 ui 命令超时秒（默认 600，0 关闭）
#   UI_SOAK_SAMPLE_MS      采样间隔（默认 10ms；不支持小数 sleep 时降为 1s）
#   UI_SOAK_METRIC         auto|gnu|proc|ps（默认 auto；强制某路径用于降级验收）
#   UI_SOAK_WS/LOG/STATE   工作区/日志/状态文件
set -euo pipefail

# Windows+core.autocrlf=true 的 worktree 会把 env.sh 写成 CRLF，bash 无法解析；
# env.sh 不在本次改动范围，这里用 /tmp 的 LF 副本加载（不改仓库文件）。
_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if grep -q $'\r' "$_here/env.sh" 2>/dev/null; then
  _env_tmp="${TMPDIR:-/tmp}/ui-env-soak-$(id -u).sh"
  tr -d '\r' < "$_here/env.sh" >"$_env_tmp"
  # shellcheck disable=SC1090
  source "$_env_tmp"
else
  # shellcheck disable=SC1091
  source "$_here/env.sh"
fi
# env.sh 用自身位置推导 UI_ROOT；从 /tmp 副本加载时会算成 /，这里按真实脚本位置纠正。
UI_ROOT="$(cd "$_here/.." && pwd)"
export UI_ROOT
export UI_GIT_DIR="$UI_ROOT"
cd "$UI_ROOT"

BIN="$UI_BIN"
WS="${UI_SOAK_WS:-/tmp/ui-soak-long}"
SECONDS_TOTAL="${UI_SOAK_SECONDS:-86400}"
INTERVAL="${UI_SOAK_INTERVAL:-1}"
LOG="${UI_SOAK_LOG:-/tmp/ui-soak-long.log}"
STATE="${UI_SOAK_STATE:-/tmp/ui-soak-long-state.json}"
ROUND_TIMEOUT="${UI_SOAK_ROUND_TIMEOUT:-600}"
SAMPLE_MS="${UI_SOAK_SAMPLE_MS:-10}"

case "$SECONDS_TOTAL" in ''|*[!0-9]*) echo "FAIL: UI_SOAK_SECONDS 必须是整数: $SECONDS_TOTAL" >&2; exit 2 ;; esac
case "$INTERVAL" in ''|*[!0-9]*) echo "FAIL: UI_SOAK_INTERVAL 必须是整数: $INTERVAL" >&2; exit 2 ;; esac
case "$ROUND_TIMEOUT" in ''|*[!0-9]*) echo "WARN: UI_SOAK_ROUND_TIMEOUT 非整数，回退 600" >&2; ROUND_TIMEOUT=600 ;; esac
case "$SAMPLE_MS" in ''|*[!0-9]*) echo "WARN: UI_SOAK_SAMPLE_MS 非整数，回退 10" >&2; SAMPLE_MS=10 ;; esac

[ -x "$BIN" ] || { echo "FAIL: ui 二进制不存在或不可执行: $BIN" >&2; exit 2; }
mkdir -p "$(dirname "$WS")" "$(dirname "$LOG")" "$(dirname "$STATE")"
ROUNDS="${UI_SOAK_ROUNDS_LOG:-$STATE.rounds.jsonl}"
mkdir -p "$(dirname "$ROUNDS")"

RESUME="${UI_SOAK_RESUME:-0}"
case "$RESUME" in
  0|1) ;;
  *) echo "FAIL: UI_SOAK_RESUME 只能是 0 或 1: $RESUME" >&2; exit 2 ;;
esac
state_field() {
  python3 - "$STATE" "$1" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception:
    raise SystemExit(2)
v = d.get(sys.argv[2])
print("" if v is None else v)
PY
}

if [ "$RESUME" = "1" ]; then
  [ -f "$STATE" ] || { echo "FAIL: 没有可续跑的 state: $STATE" >&2; exit 2; }
  command -v python3 >/dev/null 2>&1 || { echo "FAIL: 续跑需要 python3" >&2; exit 2; }
  phase=$(state_field phase) || { echo "FAIL: state 不是合法 JSON: $STATE" >&2; exit 2; }
  case "$phase" in
    passed) echo "已通过，无需续跑: $STATE"; exit 0 ;;
    failed)
      if [ "${UI_SOAK_RESUME_FAILED:-0}" != "1" ]; then
        echo "FAIL: 上次 state=phase=failed；确认要续跑请加 UI_SOAK_RESUME_FAILED=1" >&2
        exit 2
      fi
      ;;
    running|interrupted) ;;
    *) echo "FAIL: 未知 phase=$phase" >&2; exit 2 ;;
  esac
  prev_rounds=$(state_field rounds)
  prev_start=$(state_field startedAt)
  prev_updated=$(state_field updatedAt)
  prev_seconds=$(state_field seconds)
  prev_run_id=$(state_field runId)
  prev_binary=$(state_field binary)
  prev_failed=$(state_field failedRounds)
  case "$prev_rounds$prev_start$prev_updated$prev_seconds" in
    *[!0-9]*|'') echo "FAIL: state 缺少数值字段，拒绝续跑" >&2; exit 2 ;;
  esac
  [ -n "$prev_failed" ] || prev_failed=0
  [ "$prev_seconds" = "$SECONDS_TOTAL" ] || {
    echo "WARN: 续跑沿用 state 的 seconds=$prev_seconds（当前环境是 $SECONDS_TOTAL）" >&2
    SECONDS_TOTAL="$prev_seconds"
  }
  [ -z "$prev_binary" ] || [ "$prev_binary" = "$BIN" ] || {
    echo "FAIL: 续跑的二进制路径变了: $prev_binary -> $BIN" >&2
    exit 2
  }
  active=$((prev_updated - prev_start))
  [ "$active" -ge 0 ] || active=0
  # 停机时间不算已跑时间：新 start 向前回拨 active 秒，deadline 只补剩余活跃时长。
  start=$(( $(date +%s) - active ))
  deadline=$((start + SECONDS_TOTAL))
  i="$prev_rounds"
  RUN_ID="$prev_run_id"
  [ -n "$RUN_ID" ] || RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
  STATE_FAILED_ROUNDS="$prev_failed"
  printf 'soak resume runId=%s rounds=%s active=%ss seconds=%s workspace=%s\n' \
    "$RUN_ID" "$i" "$active" "$SECONDS_TOTAL" "$WS" | tee -a "$LOG"
else
  rm -rf "$WS"
  mkdir -p "$WS"
  : >"$LOG"
  : >"$ROUNDS"
  start=$(date +%s)
  deadline=$((start + SECONDS_TOTAL))
  i=0
  RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
fi

# ── 平台/指标路径探测 ────────────────────────────────────────────────
HAVE_PROC=0
if [ -d /proc/self ]; then HAVE_PROC=1; fi
HAVE_GNU_TIME=0
if [ -x /usr/bin/time ] && /usr/bin/time -v true >/dev/null 2>&1; then
  HAVE_GNU_TIME=1
fi
HAVE_LSOF=0
if command -v lsof >/dev/null 2>&1; then HAVE_LSOF=1; fi

METRIC_MODE="${UI_SOAK_METRIC:-auto}"
case "$METRIC_MODE" in
  auto)
    if [ "$HAVE_GNU_TIME" = 1 ]; then METRIC_MODE=gnu
    elif [ "$HAVE_PROC" = 1 ]; then METRIC_MODE=proc
    else METRIC_MODE=ps; fi
    ;;
  gnu|proc|ps) ;;
  *) echo "WARN: 未知 UI_SOAK_METRIC=$METRIC_MODE，按 auto 处理" >&2; METRIC_MODE=auto ;;
esac
if [ "$METRIC_MODE" = gnu ] && [ "$HAVE_GNU_TIME" != 1 ]; then
  echo "WARN: 要求 gnu 但 /usr/bin/time -v 不可用，降级为 $([ "$HAVE_PROC" = 1 ] && echo proc || echo ps)" >&2
  METRIC_MODE=$([ "$HAVE_PROC" = 1 ] && echo proc || echo ps)
fi
if [ "$METRIC_MODE" = proc ] && [ "$HAVE_PROC" != 1 ]; then
  echo "WARN: 要求 proc 但没有 /proc，降级为 ps" >&2
  METRIC_MODE=ps
fi

# 采样间隔：GNU sleep 支持小数，BSD sleep 只认整数。
if ! sleep 0.1 2>/dev/null; then
  POLL_SLEEP=1
else
  POLL_SLEEP="$(awk -v ms="$SAMPLE_MS" 'BEGIN{printf "%.3f", ms/1000}')"
fi
# 还没发现被测子进程时用更短的间隔：ui verify 只有几十毫秒，等 10ms 就可能错过。
if sleep 0.001 2>/dev/null; then
  DISCOVER_SLEEP=0.001
else
  DISCOVER_SLEEP="$POLL_SLEEP"
fi

if [ "$METRIC_MODE" = gnu ]; then
  PRIMARY_RSS_SRC="gnu-time-v"
elif [ "$METRIC_MODE" = proc ]; then
  PRIMARY_RSS_SRC="proc-tree-rss"
else
  PRIMARY_RSS_SRC="ps-tree-rss"
fi
if [ "$HAVE_PROC" = 1 ] && [ "$METRIC_MODE" != ps ]; then
  PRIMARY_FD_SRC="proc-fd-tree"
elif [ "$HAVE_LSOF" = 1 ]; then
  PRIMARY_FD_SRC="lsof-fd-tree"
else
  PRIMARY_FD_SRC="na"
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ui-soak-metrics.XXXXXX")"
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT

# ── 工具函数 ────────────────────────────────────────────────────────
json_escape() {
  local s=${1-}
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\r'/\\r}
  s=${s//$'\t'/\\t}
  printf '%s' "$s"
}

now_ms() {
  local t
  t=$(date +%s%3N 2>/dev/null || true)
  case "$t" in
    ''|*[!0-9]*) printf '%s000' "$(date +%s)" ;;
    *) printf '%s' "$t" ;;
  esac
}

_children_of() {
  local pid=$1 kids=""
  local f="/proc/$pid/task/$pid/children"
  if [ -r "$f" ]; then
    # 纯 bash 内建读取，避免每次采样 fork cat。
    read -r kids <"$f" 2>/dev/null || true
    printf '%s' "$kids"
  elif command -v ps >/dev/null 2>&1; then
    ps -o pid= --ppid "$pid" 2>/dev/null | tr -d ' \n' | sed 's/\([0-9][0-9]*\)/\1 /g' || true
  fi
}

# 输出: "<RSS_KB> <FD_COUNT> <PROC_COUNT>"，RSS 为当前瞬时进程树之和。
# 全部用 bash 内建读取 /proc，不 fork awk/ls/wc：ui verify 只有几十毫秒，
# fork 采样器本身就要几十毫秒，会几乎采不到点。
sample_proc_tree() {
  local root=$1
  local -a queue=("$root")
  local rss=0 fds=0 procs=0 pid v line kids
  local -a fdentries
  while [ "${#queue[@]}" -gt 0 ]; do
    pid="${queue[0]}"
    queue=("${queue[@]:1}")
    [ -d "/proc/$pid" ] || continue
    procs=$((procs + 1))
    if [ -r "/proc/$pid/status" ]; then
      while IFS= read -r line; do
        case "$line" in
          VmRSS:*)
            v=${line#VmRSS:}
            v=${v//[!0-9]/}
            rss=$((rss + ${v:-0}))
            break
            ;;
        esac
      done <"/proc/$pid/status" 2>/dev/null || true
    fi
    fdentries=(/proc/"$pid"/fd/*)
    if [ -L "${fdentries[0]:-}" ]; then
      fds=$((fds + ${#fdentries[@]}))
    fi
    kids=$(_children_of "$pid" || true)
    for v in $kids; do queue+=("$v"); done
  done
  printf '%s %s %s\n' "$rss" "$fds" "$procs"
}

# macOS/无 /proc：用 ps 建 pid→ppid 图，求进程树 RSS（KB）。
sample_ps_tree() {
  local root=$1
  ps -eo pid=,ppid=,rss= 2>/dev/null | awk -v root="$root" '
    { pid[NR]=$1; pp[NR]=$2; rss[NR]=$3 }
    END {
      mark[root]=1
      do {
        changed=0
        for (k=1;k<=NR;k++) {
          if (!(pid[k] in mark) && (pp[k] in mark)) { mark[pid[k]]=1; changed=1 }
        }
      } while (changed)
      r=0; n=0
      for (k=1;k<=NR;k++) if (pid[k] in mark) { r+=rss[k]; n++ }
      printf "%d 0 %d\n", r, n
    }'
}

ps_tree_pids() {
  local root=$1
  ps -eo pid=,ppid= 2>/dev/null | awk -v root="$root" '
    { pid[NR]=$1; pp[NR]=$2 }
    END {
      mark[root]=1
      do {
        changed=0
        for (k=1;k<=NR;k++) {
          if (!(pid[k] in mark) && (pp[k] in mark)) { mark[pid[k]]=1; changed=1 }
        }
      } while (changed)
      for (k=1;k<=NR;k++) if (pid[k] in mark) print pid[k]
    }'
}

sample_lsof_tree() {
  local root=$1 total=0 p c
  [ "$HAVE_LSOF" = 1 ] || { printf '0\n'; return; }
  for p in $(ps_tree_pids "$root"); do
    c=$(lsof -p "$p" 2>/dev/null | awk 'NR>1' | wc -l || true)
    c=${c//[!0-9]/}
    total=$((total + ${c:-0}))
  done
  printf '%s\n' "$total"
}

sample_now() {
  local target=$1 line rss=0 fds=0 procs=0
  if [ "$HAVE_PROC" = 1 ] && [ -d "/proc/$target" ]; then
    line=$(sample_proc_tree "$target")
    read -r rss fds procs <<<"$line" || true
  else
    line=$(sample_ps_tree "$target")
    read -r rss fds procs <<<"$line" || true
    if [ "$HAVE_LSOF" = 1 ]; then
      fds=$(sample_lsof_tree "$target")
    else
      fds=0
    fi
  fi
  printf '%s %s %s\n' "${rss:-0}" "${fds:-0}" "${procs:-0}"
}

kill_tree() {
  local root=$1 sig=$2
  local -a all=("$root") queue=("$root")
  local pid kids c j
  while [ "${#queue[@]}" -gt 0 ]; do
    pid="${queue[0]}"
    queue=("${queue[@]:1}")
    kids=$(_children_of "$pid" || true)
    for c in $kids; do
      all+=("$c")
      queue+=("$c")
    done
  done
  for ((j = ${#all[@]} - 1; j >= 0; j--)); do
    kill -"$sig" "${all[j]}" 2>/dev/null || true
  done
}

metric_reset() {
  METRIC_RSS_KB=0
  METRIC_FD_MAX=0
  METRIC_PROCS=0
  METRIC_RSS_SRC="na"
  METRIC_FD_SRC="na"
  METRIC_TIMED_OUT=0
  METRIC_RC=0
  METRIC_MS=0
  METRIC_LABEL=""
  METRIC_RSS_POLLED=0
}

metric_json() {
  local timed=false
  if [ "$METRIC_TIMED_OUT" = 1 ]; then timed=true; fi
  printf '{"rc":%s,"ms":%s,"rssKB":%s,"rssSource":"%s","fdMax":%s,"fdSource":"%s","procsPeak":%s,"timedOut":%s}' \
    "$METRIC_RC" "$METRIC_MS" "$METRIC_RSS_KB" "$(json_escape "$METRIC_RSS_SRC")" \
    "$METRIC_FD_MAX" "$(json_escape "$METRIC_FD_SRC")" "$METRIC_PROCS" "$timed"
}

metric_log() {
  printf 'metrics stage=%s round=%s rc=%s ms=%s rssKB=%s rssSource=%s fdMax=%s fdSource=%s procsPeak=%s timedOut=%s rssPolledPeakKB=%s runnerShellPid=%s\n' \
    "$METRIC_LABEL" "$i" "$METRIC_RC" "$METRIC_MS" "$METRIC_RSS_KB" "$METRIC_RSS_SRC" \
    "$METRIC_FD_MAX" "$METRIC_FD_SRC" "$METRIC_PROCS" "$METRIC_TIMED_OUT" "$METRIC_RSS_POLLED" "$$" |
    tee -a "$LOG"
}

# 运行一条被测命令：stdout/stderr 追加进 $LOG，采集真实子进程树的 RSS/fd。
measure() {
  local label=$1
  shift
  metric_reset
  METRIC_LABEL="$label"
  local mfile="$TMP_DIR/time-$label.$$"
  rm -f "$mfile"
  local start_ms root=0 rc=0 uipid=""
  start_ms=$(now_ms)

  if [ "$METRIC_MODE" = gnu ]; then
    METRIC_RSS_SRC="gnu-time-v"
    METRIC_FD_SRC="$PRIMARY_FD_SRC"
    # GNU time 直接 fork/exec 被测命令，它的第一个子进程就是 ui；不需要 shell shim，
    # 少一次 fork/exec 就能在几十毫秒的 ui verify 里采到 fd。
    /usr/bin/time -v -o "$mfile" "$@" >>"$LOG" 2>&1 &
    root=$!
  else
    METRIC_RSS_SRC="$PRIMARY_RSS_SRC"
    METRIC_FD_SRC="$PRIMARY_FD_SRC"
    "$@" >>"$LOG" 2>&1 &
    root=$!
    uipid=$root
  fi

  local deadline_at=0 rss_peak=0 fd_peak=0 proc_peak=0 iter=0
  local rss=0 fds=0 procs=0 sample target kids k
  if [ "$ROUND_TIMEOUT" -gt 0 ]; then
    deadline_at=$((SECONDS + ROUND_TIMEOUT))
  fi
  while kill -0 "$root" 2>/dev/null; do
    iter=$((iter + 1))
    if [ -z "$uipid" ]; then
      if [ "$HAVE_PROC" = 1 ]; then
        kids=$(_children_of "$root" || true)
        for k in $kids; do uipid=$k; break; done
      elif command -v pgrep >/dev/null 2>&1; then
        uipid=$(pgrep -P "$root" 2>/dev/null | head -1 || true)
      fi
    fi
    # 只在拿到真实被测 PID 后采样：/usr/bin/time 自己的 RSS/fd 不算 ui 的。
    if [ -n "$uipid" ]; then
      target="$uipid"
      sample=$(sample_now "$target" 2>/dev/null || true)
      read -r rss fds procs <<<"$sample" || true
      if [ "${rss:-0}" -gt "$rss_peak" ]; then rss_peak=$rss; fi
      if [ "${fds:-0}" -gt "$fd_peak" ]; then fd_peak=$fds; fi
      if [ "${procs:-0}" -gt "$proc_peak" ]; then proc_peak=$procs; fi
      if [ "${UI_SOAK_DEBUG:-0}" = 1 ]; then
        printf 'debug stage=%s iter=%s t=%sms root=%s uipid=%s target=%s sample="%s"\n' \
          "$label" "$iter" "$(( $(now_ms) - start_ms ))" "$root" "${uipid:-}" "$target" "${sample:-}" >>"$LOG"
      fi
    fi
    if [ "$deadline_at" -gt 0 ] && [ "$SECONDS" -ge "$deadline_at" ]; then
      METRIC_TIMED_OUT=1
      kill_tree "${uipid:-$root}" TERM
      kill "$root" 2>/dev/null || true
      sleep 1 2>/dev/null || true
      kill_tree "${uipid:-$root}" KILL
      kill -KILL "$root" 2>/dev/null || true
      break
    fi
    if [ -z "$uipid" ]; then
      sleep "$DISCOVER_SLEEP"
    else
      sleep "$POLL_SLEEP"
    fi
  done
  wait "$root" 2>/dev/null || rc=$?
  if [ "$METRIC_TIMED_OUT" = 1 ]; then
    rc=124
  fi
  METRIC_RC=$rc
  METRIC_RSS_POLLED=$rss_peak
  METRIC_PROCS=$proc_peak
  METRIC_MS=$(( $(now_ms) - start_ms ))

  # RSS 归属：优先 GNU time 的 maxrss；没有它才用我们自己轮询的峰值。
  if [ "$METRIC_MODE" = gnu ] && [ -r "$mfile" ]; then
    local maxrss
    maxrss=$(awk -F': ' '/Maximum resident set size/ {print $2; exit}' "$mfile" 2>/dev/null | tr -d ' ' || true)
    case "$maxrss" in
      ''|*[!0-9]*) maxrss=0 ;;
    esac
    if [ "${maxrss:-0}" -gt 0 ]; then
      METRIC_RSS_KB=$maxrss
      METRIC_RSS_SRC="gnu-time-v"
    elif [ "$rss_peak" -gt 0 ]; then
      METRIC_RSS_KB=$rss_peak
      METRIC_RSS_SRC="proc-tree-rss-fallback"
    else
      METRIC_RSS_KB=0
      METRIC_RSS_SRC="na"
    fi
  else
    if [ "$rss_peak" -gt 0 ]; then
      METRIC_RSS_KB=$rss_peak
    else
      METRIC_RSS_KB=0
      METRIC_RSS_SRC="na"
    fi
  fi

  if [ "$fd_peak" -gt 0 ]; then
    METRIC_FD_MAX=$fd_peak
  else
    METRIC_FD_MAX=0
    METRIC_FD_SRC="na"
  fi

  metric_log
}

# ── state（原子写） ─────────────────────────────────────────────────
LAST_ROUND_JSON=null
STATE_LAST_ERROR=""
STATE_FAILED_ROUNDS=0

state() {
  local phase=$1 tmp="$STATE.tmp.$$" finished_json=null err_json=null
  case "$phase" in
    passed|failed|interrupted) finished_json=$(date +%s) ;;
  esac
  if [ -n "$STATE_LAST_ERROR" ]; then
    err_json="\"$(json_escape "$STATE_LAST_ERROR")\""
  fi
  printf '{"schemaVersion":2,"phase":"%s","rounds":%s,"startedAt":%s,"updatedAt":%s,"finishedAt":%s,"seconds":%s,"interval":%s,"runId":"%s","runnerPid":%s,"workspace":"%s","binary":"%s","log":"%s","roundsLog":"%s","metricMode":"%s","metrics":{"rss":"ui-process-tree","rssSource":"%s","fdSource":"%s"},"lastRound":%s,"lastError":%s,"failedRounds":%s}\n' \
    "$phase" "$i" "$start" "$(date +%s)" "$finished_json" "$SECONDS_TOTAL" "$INTERVAL" \
    "$RUN_ID" "$$" "$(json_escape "$WS")" "$(json_escape "$BIN")" "$(json_escape "$LOG")" \
    "$(json_escape "$ROUNDS")" "$METRIC_MODE" "$PRIMARY_RSS_SRC" "$PRIMARY_FD_SRC" \
    "$LAST_ROUND_JSON" "$err_json" "$STATE_FAILED_ROUNDS" >"$tmp"
  mv -f "$tmp" "$STATE"
}

record_round() {
  local ok=$1 run_json=$2 verify_json=$3 chain=$4
  LAST_ROUND_JSON="{\"round\":$i,\"ok\":$ok,\"run\":$run_json,\"verify\":$verify_json,\"journalChainOk\":$chain}"
  printf '%s\n' "$LAST_ROUND_JSON" >>"$ROUNDS"
}

fail_round() {
  local stage=$1 reason=$2
  STATE_FAILED_ROUNDS=$((STATE_FAILED_ROUNDS + 1))
  STATE_LAST_ERROR="round=$i stage=$stage reason=$reason"
  printf 'RESULT round=%s status=fail stage=%s reason=%s rssKB=%s rssSource=%s fdMax=%s fdSource=%s runMs=%s verifyMs=%s timedOut=%s log=%s state=%s\n' \
    "$i" "$stage" "$(json_escape "$reason")" "$METRIC_RSS_KB" "$METRIC_RSS_SRC" \
    "$METRIC_FD_MAX" "$METRIC_FD_SRC" "$RUN_MS" "$VERIFY_MS" "$METRIC_TIMED_OUT" "$LOG" "$STATE" |
    tee -a "$LOG"
  state failed
  echo "FAIL: $reason（轮次 $i，stage=$stage）" >&2
  echo "FAIL 明细: $LOG / $STATE / $ROUNDS" >&2
  exit 1
}

# ── 启动 ────────────────────────────────────────────────────────────
state running
{
  printf 'soak start runId=%s seconds=%s interval=%s roundTimeout=%s sampleMs=%s\n' \
    "$RUN_ID" "$SECONDS_TOTAL" "$INTERVAL" "$ROUND_TIMEOUT" "$SAMPLE_MS"
  printf 'metricMode=%s rss=%s(primary=%s) fd=%s runnerShellPid=%s (shell RSS 不参与指标)\n' \
    "$METRIC_MODE" "ui-process-tree" "$PRIMARY_RSS_SRC" "$PRIMARY_FD_SRC" "$$"
  printf 'workspace=%s log=%s state=%s roundsLog=%s\n' "$WS" "$LOG" "$STATE" "$ROUNDS"
} | tee -a "$LOG"

on_signal() {
  STATE_LAST_ERROR="interrupted by signal (round=$i)"
  state interrupted
  exit 130
}
trap on_signal INT TERM

# ── 主循环 ──────────────────────────────────────────────────────────
RUN_MS=0
VERIFY_MS=0
RUN_RSS=0
RUN_RSS_SRC="na"
RUN_FD=0
RUN_FD_SRC="na"
VERIFY_RSS=0
VERIFY_RSS_SRC="na"
VERIFY_FD=0
VERIFY_FD_SRC="na"
while [ "$(date +%s)" -lt "$deadline" ]; do
  i=$((i + 1))

  measure run "$BIN" run --workspace "$WS" --task "soak-long-$i" \
    --mock "$UI_ROOT/fixtures/mock.json" --yes
  RUN_JSON=$(metric_json)
  RUN_MS=$METRIC_MS
  RUN_RSS=$METRIC_RSS_KB
  RUN_RSS_SRC=$METRIC_RSS_SRC
  RUN_FD=$METRIC_FD_MAX
  RUN_FD_SRC=$METRIC_FD_SRC
  if [ "$METRIC_RC" -ne 0 ]; then
    record_round false "$RUN_JSON" null null
    fail_round run "ui run rc=$METRIC_RC timedOut=$METRIC_TIMED_OUT"
  fi

  # 只看本轮 verify 新增的日志，避免拿上一轮的『哈希链完整』蒙混过关。
  log_mark=$(wc -c <"$LOG" | tr -d ' ')
  measure verify "$BIN" verify --workspace "$WS"
  VERIFY_JSON=$(metric_json)
  VERIFY_MS=$METRIC_MS
  VERIFY_RSS=$METRIC_RSS_KB
  VERIFY_RSS_SRC=$METRIC_RSS_SRC
  VERIFY_FD=$METRIC_FD_MAX
  VERIFY_FD_SRC=$METRIC_FD_SRC
  if [ "$METRIC_RC" -ne 0 ]; then
    record_round false "$RUN_JSON" "$VERIFY_JSON" false
    fail_round verify "ui verify rc=$METRIC_RC timedOut=$METRIC_TIMED_OUT"
  fi
  round_out=$(tail -c +"$((log_mark + 1))" "$LOG" 2>/dev/null || true)
  if ! grep -q '哈希链完整' <<<"$round_out"; then
    record_round false "$RUN_JSON" "$VERIFY_JSON" false
    fail_round verify "本轮 verify 输出没有『哈希链完整』（journal 链可能损坏）"
  fi

  record_round true "$RUN_JSON" "$VERIFY_JSON" true
  printf 'round=%s elapsed=%ss status=ok runRss=%sKB(%s) runFd=%s(%s) runMs=%s verifyRss=%sKB(%s) verifyFd=%s(%s) verifyMs=%s\n' \
    "$i" "$(( $(date +%s) - start ))" \
    "$RUN_RSS" "$RUN_RSS_SRC" "$RUN_FD" "$RUN_FD_SRC" "$RUN_MS" \
    "$VERIFY_RSS" "$VERIFY_RSS_SRC" "$VERIFY_FD" "$VERIFY_FD_SRC" "$VERIFY_MS" |
    tee -a "$LOG"
  state running

  sleep "$INTERVAL"
done

# 只找真正还在跑 ui 的进程；不要匹配父 shell 命令行里的路径字符串。
if command -v pgrep >/dev/null 2>&1 && pgrep -f "ui run --workspace $WS" >/dev/null 2>&1; then
  STATE_LAST_ERROR="仍有 ui run --workspace $WS 进程残留"
  state failed
  echo "FAIL: 仍有 ui run --workspace $WS 进程" >&2
  pgrep -af "ui run --workspace $WS" >&2 || true
  exit 1
fi
state passed
echo "PASS soak-long rounds=$i seconds=$SECONDS_TOTAL state=$STATE metrics=rss:$PRIMARY_RSS_SRC,fd:$PRIMARY_FD_SRC"
