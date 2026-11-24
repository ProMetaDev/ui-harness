#!/usr/bin/env bash
# 真实 provider 长跑：每轮真模型 + 真工具执行（ui run → ui verify）。
#
# 和 tools/soak_long.sh 的分工（两者不能互相替代）：
#   soak_long.sh  mock 剧本 + 24h 进程/journal/RSS 指标，零成本、可进 CI；
#   soak_real.sh  真 provider + 真工具调用，回答「模型到底会不会动手、日志能不能验」。
#   mock 绿了不证明 provider 可用；真跑几轮也不证明 24h 稳定。
#
# 默认 SKIP（不产生任何模型调用、不花钱）：
#   只有 UI_MODEL_CMD 非空 **且** UI_SOAK_ALLOW_COST=1 才会真正跑。
#
# 成本可见：开跑前打印轮数、模型调用下限/常见值、最坏墙钟和预算参数；
#   每轮记录 tokens/工具数/耗时；不自动重试，任何一轮失败立即退出。
#
# 每轮判定（provider API 无关，只看 journal）：
#   1. ui run 退出码 0，且结束原因 done；
#   2. ui verify 退出码 0，且输出「哈希链完整」；
#   3. 本轮新增 journal 里至少一次成功的 fs_write/fs_edit/fs_mv 或 shell
#      （默认任务还要求 soak.txt 非空）。
#
# 可用环境变量：
#   UI_SOAK_ALLOW_COST=1   成本闸，必须显式打开
#   UI_MODEL_CMD           模型进程命令，必填；只要能从 stdin 读快照、向 stdout
#                          写 {"content","toolCalls"} 就行，不绑定具体 provider
#   UI_SOAK_ROUNDS         轮数（默认 3）
#   UI_SOAK_INTERVAL       轮间隔秒（默认 10）
#   UI_SOAK_WS             工作区（默认 /tmp/ui-soak-real；默认开跑前清空）
#   UI_SOAK_LOG            日志（默认 /tmp/ui-soak-real.log）
#   UI_SOAK_STATE          状态 JSON（默认 /tmp/ui-soak-real-state.json）
#   UI_SOAK_TASK           任务文本（默认：创建/修改 soak.txt 并读回确认）
#   UI_SOAK_MODEL_LABEL    写进日志/state 的模型标签（不要放密钥）
#   UI_SOAK_RUN_TIMEOUT    单轮 ui run 超时秒（默认 1800；0 关闭）
#   UI_SOAK_MAX_TOKENS / UI_SOAK_MAX_CALLS / UI_SOAK_MAX_STEPS /
#   UI_SOAK_MAX_USD / UI_SOAK_USD_PER_MTOK
#                          传给 ui run 的预算上限（不设就用 ui 默认）
#   UI_SOAK_REQUIRE_TOOL   1（默认）要求工具证据；0 只记录不判定
#   UI_SOAK_KEEP_WS=1      保留已有 workspace（默认清空）
#
# 退出码：0=passed 或 skipped；1=某轮 failed；2=配置/环境错误；130=被中断。
set -euo pipefail

# Windows+core.autocrlf=true 的 worktree 会把 env.sh 写成 CRLF，bash 无法解析；
# env.sh 不在本次改动范围，这里用 /tmp 的 LF 副本加载（不改仓库文件）。
_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if grep -q $'\r' "$_here/env.sh" 2>/dev/null; then
  _env_tmp="${TMPDIR:-/tmp}/ui-env-soak-real-$(id -u).sh"
  tr -d '\r' <"$_here/env.sh" >"$_env_tmp"
  # shellcheck disable=SC1090
  source "$_env_tmp"
else
  # shellcheck disable=SC1091
  source "$_here/env.sh"
fi
# env.sh 用自身位置推导 UI_ROOT；从 /tmp 副本加载时会算成 /，这里按真实位置纠正。
UI_ROOT="$(cd "$_here/.." && pwd)"
export UI_ROOT
export UI_GIT_DIR="$UI_ROOT"
cd "$UI_ROOT"

BIN="$UI_BIN"
ROUNDS="${UI_SOAK_ROUNDS:-3}"
INTERVAL="${UI_SOAK_INTERVAL:-10}"
WS="${UI_SOAK_WS:-/tmp/ui-soak-real}"
LOG="${UI_SOAK_LOG:-/tmp/ui-soak-real.log}"
STATE="${UI_SOAK_STATE:-/tmp/ui-soak-real-state.json}"
ROUNDS_LOG="${UI_SOAK_ROUNDS_LOG:-$STATE.rounds.jsonl}"
RUN_TIMEOUT="${UI_SOAK_RUN_TIMEOUT:-1800}"
REQUIRE_TOOL="${UI_SOAK_REQUIRE_TOOL:-1}"
KEEP_WS="${UI_SOAK_KEEP_WS:-0}"
MODEL_LABEL="${UI_SOAK_MODEL_LABEL:-${UI_MODEL:-unspecified}}"

DEFAULT_TASK=0
TASK="${UI_SOAK_TASK:-}"
if [ -z "$TASK" ]; then
  DEFAULT_TASK=1
  TASK='创建或修改当前工作区的 soak.txt：写入一行以 soak round 开头的确认内容（轮次用本任务末尾括号里的数字），然后用 fs_read 读回 soak.txt，确认内容正确后说明结果。必须至少执行一次 fs_write 或 shell 真的落盘，不能只口头宣布完成。'
fi

# 成本闸：两个条件必须同时满足，缺一个都只 SKIP，不跑、不花钱。
SKIP_REASONS=()
_model_cmd_trimmed="${UI_MODEL_CMD:-}"
_model_cmd_trimmed="${_model_cmd_trimmed//[[:space:]]/}"
if [ -z "$_model_cmd_trimmed" ]; then
  SKIP_REASONS+=("UI_MODEL_CMD 为空：没有真实 provider")
fi
if [ "${UI_SOAK_ALLOW_COST:-}" != "1" ]; then
  SKIP_REASONS+=("UI_SOAK_ALLOW_COST != 1：成本闸未显式打开")
fi

case "$ROUNDS" in ''|*[!0-9]*) echo "FAIL: UI_SOAK_ROUNDS 必须是正整数: $ROUNDS" >&2; exit 2 ;; esac
case "$INTERVAL" in ''|*[!0-9]*) echo "FAIL: UI_SOAK_INTERVAL 必须是非负整数: $INTERVAL" >&2; exit 2 ;; esac
case "$RUN_TIMEOUT" in ''|*[!0-9]*) echo "FAIL: UI_SOAK_RUN_TIMEOUT 必须是非负整数: $RUN_TIMEOUT" >&2; exit 2 ;; esac
if [ "$ROUNDS" -lt 1 ]; then
  echo "FAIL: UI_SOAK_ROUNDS 至少为 1: $ROUNDS" >&2
  exit 2
fi
case "$REQUIRE_TOOL" in 0|1) ;; *) echo "FAIL: UI_SOAK_REQUIRE_TOOL 只支持 0/1: $REQUIRE_TOOL" >&2; exit 2 ;; esac
case "$KEEP_WS" in 0|1) ;; *) echo "FAIL: UI_SOAK_KEEP_WS 只支持 0/1: $KEEP_WS" >&2; exit 2 ;; esac

# 预算参数原样传给 ui run；只做「非空」校验，具体语义由 ui --help 定义。
UI_SOAK_MAX_TOKENS="${UI_SOAK_MAX_TOKENS:-}"
UI_SOAK_MAX_CALLS="${UI_SOAK_MAX_CALLS:-}"
UI_SOAK_MAX_STEPS="${UI_SOAK_MAX_STEPS:-}"
UI_SOAK_MAX_USD="${UI_SOAK_MAX_USD:-}"
UI_SOAK_USD_PER_MTOK="${UI_SOAK_USD_PER_MTOK:-}"

now_ms() {
  local t
  t=$(date +%s%3N 2>/dev/null || true)
  case "$t" in
    ''|*[!0-9]*) printf '%s000' "$(date +%s)" ;;
    *) printf '%s' "$t" ;;
  esac
}

json_escape() {
  local s=${1-}
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\r'/\\r}
  s=${s//$'\t'/\\t}
  printf '%s' "$s"
}

json_str_or_null() {
  if [ -n "${1:-}" ]; then
    printf '"%s"' "$(json_escape "$1")"
  else
    printf 'null'
  fi
}

# 读取当前 HEAD。普通 clone 直接 git；本仓库在 WSL 里是 Windows 绝对路径的
# worktree（.git 文件写 `X:/...`），git 不认，所以映射到 /mnt/x/... 再试一次。
# 只用于把 commit 写进 state/日志，失败就写 unknown，不影响长跑。
git_commit() {
  local c="" gd drive
  c=$(git -C "$UI_ROOT" rev-parse HEAD 2>/dev/null || true)
  if [ -z "$c" ] && [ -f "$UI_ROOT/.git" ]; then
    gd=$(sed -n 's/^gitdir: //p' "$UI_ROOT/.git" | tr -d '\r')
    case "$gd" in
      [A-Za-z]:/*)
        drive=$(printf '%s' "${gd%%:*}" | tr 'A-Z' 'a-z')
        gd="/mnt/$drive/${gd#*:/}"
        c=$(GIT_DIR="$gd" GIT_WORK_TREE="$UI_ROOT" git rev-parse HEAD 2>/dev/null || true)
        ;;
    esac
  fi
  printf '%s' "${c:-unknown}"
}

mkdir -p "$(dirname "$WS")" "$(dirname "$LOG")" "$(dirname "$STATE")" "$(dirname "$ROUNDS_LOG")" 2>/dev/null || true

start=$(date +%s)
RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
COMMIT="$(git_commit)"
i=0
LAST_ROUND_JSON=null
STATE_LAST_ERROR=""
STATE_FAILED_ROUNDS=0
SKIP_JSON=null
TOKENS_TOTAL=0

# SKIP 也要留一份 state：让「没跑」和「跑失败」在文件层面分得清。
if [ "${#SKIP_REASONS[@]}" -gt 0 ]; then
  SKIP_JSON="["
  first=1
  for r in "${SKIP_REASONS[@]}"; do
    if [ "$first" = 1 ]; then first=0; else SKIP_JSON+=","; fi
    SKIP_JSON+="\"$(json_escape "$r")\""
  done
  SKIP_JSON+="]"

  printf 'SKIP soak-real：真实 provider 长跑未启用（没有产生任何模型调用）\n'
  for r in "${SKIP_REASONS[@]}"; do
    printf '  - %s\n' "$r"
  done
  printf '启用方式（会真实计费）：\n'
  printf '  UI_SOAK_ALLOW_COST=1 UI_MODEL_CMD="node tools/chat_compat.mjs" bash tools/soak_real.sh\n'
  printf '说明：docs/50-real-soak.md\n'

  tmp="$STATE.tmp.$$"
  if printf '{"schemaVersion":1,"phase":"skipped","rounds":0,"roundsPlanned":%s,"startedAt":%s,"updatedAt":%s,"finishedAt":%s,"interval":%s,"runId":"%s","runnerPid":%s,"workspace":"%s","log":"%s","roundsLog":"%s","binary":"%s","commit":"%s","modelLabel":"%s","task":"%s","defaultTask":%s,"allowCost":%s,"requireTool":%s,"runTimeout":%s,"lastRound":null,"lastError":null,"failedRounds":0,"skipReasons":%s}\n' \
    "$ROUNDS" "$start" "$(date +%s)" "$(date +%s)" "$INTERVAL" "$RUN_ID" "$$" \
    "$(json_escape "$WS")" "$(json_escape "$LOG")" "$(json_escape "$ROUNDS_LOG")" \
    "$(json_escape "$BIN")" "$(json_escape "$COMMIT")" "$(json_escape "$MODEL_LABEL")" \
    "$(json_escape "$TASK")" "$([ "$DEFAULT_TASK" = 1 ] && echo true || echo false)" \
    "$([ "${UI_SOAK_ALLOW_COST:-}" = 1 ] && echo true || echo false)" \
    "$([ "$REQUIRE_TOOL" = 1 ] && echo true || echo false)" "$RUN_TIMEOUT" "$SKIP_JSON" >"$tmp" 2>/dev/null; then
    mv -f "$tmp" "$STATE" 2>/dev/null || rm -f "$tmp"
  fi
  exit 0
fi

# ── 配置检查（此时确定要跑，配置错误按 failed 记 state） ──────────────
config_fail() {
  STATE_LAST_ERROR="$1"
  printf 'FAIL: %s\n' "$1" >&2
  tmp="$STATE.tmp.$$"
  if printf '{"schemaVersion":1,"phase":"failed","rounds":0,"roundsPlanned":%s,"startedAt":%s,"updatedAt":%s,"finishedAt":%s,"interval":%s,"runId":"%s","runnerPid":%s,"workspace":"%s","log":"%s","roundsLog":"%s","binary":"%s","commit":"%s","modelLabel":"%s","task":"%s","defaultTask":%s,"allowCost":true,"requireTool":%s,"runTimeout":%s,"lastRound":null,"lastError":"%s","failedRounds":0,"skipReasons":null}\n' \
    "$ROUNDS" "$start" "$(date +%s)" "$(date +%s)" "$INTERVAL" "$RUN_ID" "$$" \
    "$(json_escape "$WS")" "$(json_escape "$LOG")" "$(json_escape "$ROUNDS_LOG")" \
    "$(json_escape "$BIN")" "$(json_escape "$COMMIT")" "$(json_escape "$MODEL_LABEL")" \
    "$(json_escape "$TASK")" "$([ "$DEFAULT_TASK" = 1 ] && echo true || echo false)" \
    "$([ "$REQUIRE_TOOL" = 1 ] && echo true || echo false)" "$RUN_TIMEOUT" \
    "$(json_escape "$STATE_LAST_ERROR")" >"$tmp" 2>/dev/null; then
    mv -f "$tmp" "$STATE" 2>/dev/null || rm -f "$tmp"
  fi
  exit 2
}

[ -x "$BIN" ] || config_fail "ui 二进制不存在或不可执行: $BIN（先 cargo build --release，或检查 UI_BIN）"
command -v python3 >/dev/null 2>&1 || config_fail "需要 python3 解析 journal 事件（只用于统计，不联网）"

# 防止手滑把 UI_SOAK_WS 指到家目录/仓库根：脚本默认会 rm -rf 工作区。
mkdir -p "$(dirname "$WS")" 2>/dev/null || config_fail "无法创建 workspace 父目录: $(dirname "$WS")"
ws_real="$(cd "$(dirname "$WS")" && pwd -P)/$(basename "$WS")"
case "$ws_real" in
  / | "$HOME" | "$UI_ROOT" | "$UI_ROOT"/*)
    config_fail "UI_SOAK_WS 指向受保护路径（会被清空）: $ws_real；请用 /tmp 下的独立目录"
    ;;
esac
WS="$ws_real"

# ── 工具函数 ─────────────────────────────────────────────────────────
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ui-soak-real.XXXXXX")"
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT

state() {
  local phase=$1 tmp="$STATE.tmp.$$" finished_json=null err_json=null
  case "$phase" in
    passed|failed|skipped) finished_json=$(date +%s) ;;
  esac
  if [ -n "$STATE_LAST_ERROR" ]; then
    err_json="\"$(json_escape "$STATE_LAST_ERROR")\""
  fi
  printf '{"schemaVersion":1,"phase":"%s","rounds":%s,"roundsPlanned":%s,"startedAt":%s,"updatedAt":%s,"finishedAt":%s,"interval":%s,"runId":"%s","runnerPid":%s,"workspace":"%s","log":"%s","roundsLog":"%s","binary":"%s","commit":"%s","modelLabel":"%s","task":"%s","defaultTask":%s,"allowCost":true,"requireTool":%s,"runTimeout":%s,"budget":{"maxTokens":%s,"maxCalls":%s,"maxSteps":%s,"maxUsd":%s,"usdPerMtok":%s},"tokensTotal":%s,"lastRound":%s,"lastError":%s,"failedRounds":%s,"skipReasons":null}\n' \
    "$phase" "$i" "$ROUNDS" "$start" "$(date +%s)" "$finished_json" "$INTERVAL" \
    "$RUN_ID" "$$" "$(json_escape "$WS")" "$(json_escape "$LOG")" "$(json_escape "$ROUNDS_LOG")" \
    "$(json_escape "$BIN")" "$(json_escape "$COMMIT")" "$(json_escape "$MODEL_LABEL")" \
    "$(json_escape "$TASK")" "$([ "$DEFAULT_TASK" = 1 ] && echo true || echo false)" \
    "$([ "$REQUIRE_TOOL" = 1 ] && echo true || echo false)" "$RUN_TIMEOUT" \
    "$(json_str_or_null "$UI_SOAK_MAX_TOKENS")" "$(json_str_or_null "$UI_SOAK_MAX_CALLS")" \
    "$(json_str_or_null "$UI_SOAK_MAX_STEPS")" "$(json_str_or_null "$UI_SOAK_MAX_USD")" \
    "$(json_str_or_null "$UI_SOAK_USD_PER_MTOK")" "$TOKENS_TOTAL" \
    "$LAST_ROUND_JSON" "$err_json" "$STATE_FAILED_ROUNDS" >"$tmp"
  mv -f "$tmp" "$STATE"
}

# journal 增量统计：事件数 + 成功工具调用。用 python 解析，避免 grep 误命中
# 提示词正文里的工具名。offset 是本轮开始前的字节数，只读新增部分（长跑不 O(n²)）。
# 输出一行：events writes shells reads
journal_stats() {
  local offset=$1
  python3 - "$WS/.ui/journal.jsonl" "$offset" <<'PY'
import json
import sys

path, offset = sys.argv[1], int(sys.argv[2])
events = writes = shells = reads = 0
try:
    f = open(path, "rb")
except OSError:
    print("0 0 0 0")
    raise SystemExit(0)
with f:
    f.seek(offset)
    for raw in f:
        line = raw.decode("utf-8", "replace").strip()
        if not line:
            continue
        events += 1
        try:
            obj = json.loads(line)
        except Exception:
            continue
        if obj.get("kind") != "tool/end" or obj.get("ok") is not True:
            continue
        tool = obj.get("tool")
        if tool in ("fs_write", "fs_edit", "fs_mv"):
            writes += 1
        elif tool == "shell":
            shells += 1
        elif tool == "fs_read":
            reads += 1
print(events, writes, shells, reads)
PY
}

workspace_bytes() {
  local b
  b=$(du -sb "$WS" 2>/dev/null | awk '{print $1}' || true)
  case "$b" in
    ''|*[!0-9]*)
      b=$(du -sk "$WS" 2>/dev/null | awk '{print $1 * 1024}' || true)
      case "$b" in ''|*[!0-9]*) b=0 ;; esac
      ;;
  esac
  printf '%s' "$b"
}

# ══════════════════════════════════════════════════════════════════════
# 计划与成本（先打印，再动手）
# ══════════════════════════════════════════════════════════════════════
if [ "$KEEP_WS" = 1 ]; then
  mkdir -p "$WS"
else
  rm -rf "$WS"
  mkdir -p "$WS"
fi
: >"$LOG"
: >"$ROUNDS_LOG"

{
  printf 'plan soak-real runId=%s commit=%s modelLabel=%s\n' "$RUN_ID" "$COMMIT" "$MODEL_LABEL"
  printf 'plan rounds=%s interval=%ss runTimeout=%ss workspace=%s log=%s state=%s\n' \
    "$ROUNDS" "$INTERVAL" "$RUN_TIMEOUT" "$WS" "$LOG" "$STATE"
  printf 'plan task=%s\n' "$TASK"
  printf 'plan 预计模型调用：下限 %s 次（%s 轮 × 2：一次工具调用 + 一次收尾）；' "$((ROUNDS * 2))" "$ROUNDS"
  printf '默认任务常见 %s 次（%s 轮 × 3：写、读、收尾）；模型走弯路时更多。ui verify 不调模型。\n' "$((ROUNDS * 3))" "$ROUNDS"
  if [ "$RUN_TIMEOUT" -gt 0 ]; then
    printf 'plan 最坏墙钟约 %s 秒（每轮 ui run 超时 %s 秒 + verify，另加 %s 次间隔）；超时按失败退出，不重试。\n' \
      "$((ROUNDS * (RUN_TIMEOUT + 60) + (ROUNDS - 1) * INTERVAL))" "$RUN_TIMEOUT" "$((ROUNDS - 1))"
  else
    printf 'plan 最坏墙钟无上界（UI_SOAK_RUN_TIMEOUT=0）；卡住不会自动重试。\n'
  fi
  budget_desc=""
  [ -n "$UI_SOAK_MAX_TOKENS" ] && budget_desc+=" --max-tokens=$UI_SOAK_MAX_TOKENS"
  [ -n "$UI_SOAK_MAX_CALLS" ] && budget_desc+=" --max-calls=$UI_SOAK_MAX_CALLS"
  [ -n "$UI_SOAK_MAX_STEPS" ] && budget_desc+=" --max-steps=$UI_SOAK_MAX_STEPS"
  [ -n "$UI_SOAK_MAX_USD" ] && budget_desc+=" --max-usd=$UI_SOAK_MAX_USD"
  [ -n "$UI_SOAK_USD_PER_MTOK" ] && budget_desc+=" --usd-per-mtok=$UI_SOAK_USD_PER_MTOK"
  if [ -n "$budget_desc" ]; then
    printf 'plan 预算上限:%s\n' "$budget_desc"
  else
    printf 'plan 预算上限: 未设置（用 ui 默认）；真实 provider 按 token 计费，脚本不做金额兜底。\n'
  fi
  printf 'plan 失败即停、不自动重试；每轮 outcome 写入 %s，journal 在 %s/.ui/journal.jsonl\n' "$ROUNDS_LOG" "$WS"
} | tee -a "$LOG"

# 被中断时不能把正在计费的 ui 进程留在后台：先杀当前 ui，再写 failed state。
CURRENT_PID=0
on_signal() {
  local why=$1
  trap - INT TERM
  if [ "$CURRENT_PID" -gt 0 ]; then
    kill -TERM "$CURRENT_PID" 2>/dev/null || true
    sleep 1 2>/dev/null || true
    kill -KILL "$CURRENT_PID" 2>/dev/null || true
    CURRENT_PID=0
  fi
  STATE_LAST_ERROR="interrupted by signal ($why) at round=$i"
  state failed || true
  printf 'FAIL: 被信号中断（%s，round=%s），state=%s\n' "$why" "$i" "$STATE" >&2
  exit 130
}
trap 'on_signal INT' INT
trap 'on_signal TERM' TERM
state running

fail_round() {
  local stage=$1 reason=$2
  STATE_FAILED_ROUNDS=$((STATE_FAILED_ROUNDS + 1))
  STATE_LAST_ERROR="round=$i stage=$stage reason=$reason"
  state failed
  printf 'RESULT round=%s status=fail stage=%s reason=%s runMs=%s verifyMs=%s events=%s writeCalls=%s shellCalls=%s workspaceBytes=%s log=%s state=%s\n' \
    "$i" "$stage" "$reason" "$RUN_MS" "$VERIFY_MS" "$EVENTS_DELTA" "$WRITE_CALLS" "$SHELL_CALLS" "$WS_BYTES" "$LOG" "$STATE" |
    tee -a "$LOG"
  printf 'FAIL: %s（轮次 %s，stage=%s）\n' "$reason" "$i" "$stage" >&2
  printf '排查：%s（每轮有 === round N ... === 标记）\n' "$LOG" >&2
  printf '      %s/.ui/journal.jsonl；ui verify --workspace %s\n' "$WS" "$WS" >&2
  exit 1
}

# ── 主循环：每轮一次 run + 一次 verify，不重试 ───────────────────────
RUN_MS=0
VERIFY_MS=0
EVENTS_DELTA=0
WRITE_CALLS=0
SHELL_CALLS=0
WS_BYTES=0

for ((i = 1; i <= ROUNDS; i++)); do
  RUN_MS=0
  VERIFY_MS=0
  EVENTS_DELTA=0
  WRITE_CALLS=0
  SHELL_CALLS=0
  WS_BYTES=0
  journal_before=0
  if [ -f "$WS/.ui/journal.jsonl" ]; then
    journal_before=$(wc -c <"$WS/.ui/journal.jsonl" | tr -d ' ')
  fi

  round_task="$TASK"
  if [ "$DEFAULT_TASK" = 1 ]; then
    round_task="$TASK（第 $i 轮）"
  fi

  run_cmd=("$BIN" run --workspace "$WS" --task "$round_task" --yes)
  [ -n "$UI_SOAK_MAX_TOKENS" ] && run_cmd+=(--max-tokens "$UI_SOAK_MAX_TOKENS")
  [ -n "$UI_SOAK_MAX_CALLS" ] && run_cmd+=(--max-calls "$UI_SOAK_MAX_CALLS")
  [ -n "$UI_SOAK_MAX_STEPS" ] && run_cmd+=(--max-steps "$UI_SOAK_MAX_STEPS")
  [ -n "$UI_SOAK_MAX_USD" ] && run_cmd+=(--max-usd "$UI_SOAK_MAX_USD")
  [ -n "$UI_SOAK_USD_PER_MTOK" ] && run_cmd+=(--usd-per-mtok "$UI_SOAK_USD_PER_MTOK")
  if [ "$RUN_TIMEOUT" -gt 0 ]; then
    if command -v timeout >/dev/null 2>&1; then
      run_cmd=(timeout "$RUN_TIMEOUT" "${run_cmd[@]}")
    else
      printf 'WARN: 没有 timeout 命令，UI_SOAK_RUN_TIMEOUT 本轮不生效\n' | tee -a "$LOG" >&2
    fi
  fi

  run_out="$TMP_DIR/run-$i.out"
  verify_out="$TMP_DIR/verify-$i.out"
  printf '\n=== round %s ui run (task=%s) ===\n' "$i" "$round_task" >>"$LOG"
  run_start_ms=$(now_ms)
  set +e
  "${run_cmd[@]}" >"$run_out" 2>&1 &
  CURRENT_PID=$!
  wait "$CURRENT_PID"
  run_rc=$?
  set -e
  CURRENT_PID=0
  RUN_MS=$(( $(now_ms) - run_start_ms ))
  cat "$run_out" >>"$LOG"
  read -r _ev _wr _sh _rd < <(journal_stats "$journal_before" || true) || true
  _ev=${_ev:-0}; _wr=${_wr:-0}; _sh=${_sh:-0}; _rd=${_rd:-0}
  WS_BYTES=$(workspace_bytes)

  run_reason="?"
  run_steps=0
  run_tools=0
  run_tokens=0
  run_line=$(grep -E '^结束原因 ' "$run_out" | tail -n1 || true)
  if [ -n "$run_line" ]; then
    run_reason=${run_line#结束原因 }
    run_reason=${run_reason%% *}
    run_stats=$(printf '%s\n' "$run_line" | sed -n 's/.*· \([0-9][0-9]*\) 步 · \([0-9][0-9]*\) 次工具 · 约 \([0-9][0-9]*\) tokens.*/\1 \2 \3/p')
    if [ -n "$run_stats" ]; then
      read -r run_steps run_tools run_tokens <<<"$run_stats" || true
    fi
  fi
  TOKENS_TOTAL=$((TOKENS_TOTAL + run_tokens))
  if [ "$run_tokens" -eq 0 ]; then
    printf 'WARN: 本轮 ui run 报告 0 tokens；若适配器没返回 usage，这不代表免费，账单以 provider 为准。\n' |
      tee -a "$LOG" >&2
  fi
  run_json=$(printf '{"rc":%s,"ms":%s,"eventsDelta":%s,"reason":"%s","steps":%s,"toolCalls":%s,"tokens":%s}' \
    "$run_rc" "$RUN_MS" "${_ev:-0}" "$(json_escape "$run_reason")" "${run_steps:-0}" "${run_tools:-0}" "${run_tokens:-0}")

  if [ "$run_rc" -ne 0 ]; then
    EVENTS_DELTA=${_ev:-0}
    WRITE_CALLS=${_wr:-0}
    SHELL_CALLS=${_sh:-0}
    LAST_ROUND_JSON="{\"round\":$i,\"ok\":false,\"run\":$run_json,\"verify\":null,\"durationMs\":$RUN_MS,\"workspaceBytes\":$WS_BYTES,\"writeCalls\":$WRITE_CALLS,\"shellCalls\":$SHELL_CALLS,\"tokens\":${run_tokens:-0}}"
    printf '%s\n' "$LAST_ROUND_JSON" >>"$ROUNDS_LOG"
    printf '%s\n' "--- ui run 输出尾部（rc=$run_rc）---" >&2
    tail -n 25 "$run_out" >&2 || true
    if [ "$run_rc" -eq 124 ]; then
      fail_round run "ui run 超时（UI_SOAK_RUN_TIMEOUT=${RUN_TIMEOUT}s）"
    fi
    fail_round run "ui run rc=$run_rc reason=$run_reason"
  fi

  printf '\n=== round %s ui verify ===\n' "$i" >>"$LOG"
  verify_start_ms=$(now_ms)
  set +e
  "$BIN" verify --workspace "$WS" >"$verify_out" 2>&1
  verify_rc=$?
  set -e
  VERIFY_MS=$(( $(now_ms) - verify_start_ms ))
  cat "$verify_out" >>"$LOG"

  verify_events=$(grep -m1 '^事件 ' "$verify_out" | tr -cd '0-9' || true)
  verify_turns=0
  verify_tools=0
  vline=$(grep -m1 '^轮次 ' "$verify_out" || true)
  if [ -n "$vline" ]; then
    turned=${vline#轮次 }
    verify_turns=${turned%% *}
    verify_tools=${vline#*工具调用 }
    verify_tools=${verify_tools%% *}
    case "$verify_turns" in ''|*[!0-9]*) verify_turns=0 ;; esac
    case "$verify_tools" in ''|*[!0-9]*) verify_tools=0 ;; esac
  fi
  verify_json=$(printf '{"rc":%s,"ms":%s,"events":%s,"turns":%s,"toolStartsTotal":%s,"chainOk":%s}' \
    "$verify_rc" "$VERIFY_MS" "${verify_events:-0}" "$verify_turns" "$verify_tools" \
    "$([ "$verify_rc" -eq 0 ] && grep -qF '哈希链完整' "$verify_out" && echo true || echo false)")

  EVENTS_DELTA=${_ev:-0}
  WRITE_CALLS=${_wr:-0}
  SHELL_CALLS=${_sh:-0}
  if [ "$verify_rc" -ne 0 ]; then
    LAST_ROUND_JSON="{\"round\":$i,\"ok\":false,\"run\":$run_json,\"verify\":$verify_json,\"durationMs\":$((RUN_MS + VERIFY_MS)),\"workspaceBytes\":$WS_BYTES,\"writeCalls\":$WRITE_CALLS,\"shellCalls\":$SHELL_CALLS,\"tokens\":${run_tokens:-0}}"
    printf '%s\n' "$LAST_ROUND_JSON" >>"$ROUNDS_LOG"
    tail -n 20 "$verify_out" >&2 || true
    fail_round verify "ui verify rc=$verify_rc"
  fi
  if ! grep -qF '哈希链完整' "$verify_out"; then
    LAST_ROUND_JSON="{\"round\":$i,\"ok\":false,\"run\":$run_json,\"verify\":$verify_json,\"durationMs\":$((RUN_MS + VERIFY_MS)),\"workspaceBytes\":$WS_BYTES,\"writeCalls\":$WRITE_CALLS,\"shellCalls\":$SHELL_CALLS,\"tokens\":${run_tokens:-0}}"
    printf '%s\n' "$LAST_ROUND_JSON" >>"$ROUNDS_LOG"
    fail_round verify "本轮 verify 输出没有「哈希链完整」（journal 链可能损坏）"
  fi

  # provider API 无关的工具证据：只认本轮新增 journal 里的成功 tool/end。
  if [ "$REQUIRE_TOOL" = 1 ] && [ $((WRITE_CALLS + SHELL_CALLS)) -eq 0 ]; then
    LAST_ROUND_JSON="{\"round\":$i,\"ok\":false,\"run\":$run_json,\"verify\":$verify_json,\"durationMs\":$((RUN_MS + VERIFY_MS)),\"workspaceBytes\":$WS_BYTES,\"writeCalls\":$WRITE_CALLS,\"shellCalls\":$SHELL_CALLS,\"tokens\":${run_tokens:-0}}"
    printf '%s\n' "$LAST_ROUND_JSON" >>"$ROUNDS_LOG"
    fail_round evidence "本轮没有成功的 fs_write/fs_edit/fs_mv 或 shell（模型可能只说不做）"
  fi
  # 默认任务明确要求落盘 soak.txt；自定义任务不假定文件名。
  if [ "$DEFAULT_TASK" = 1 ] && [ ! -s "$WS/soak.txt" ]; then
    LAST_ROUND_JSON="{\"round\":$i,\"ok\":false,\"run\":$run_json,\"verify\":$verify_json,\"durationMs\":$((RUN_MS + VERIFY_MS)),\"workspaceBytes\":$WS_BYTES,\"writeCalls\":$WRITE_CALLS,\"shellCalls\":$SHELL_CALLS,\"tokens\":${run_tokens:-0}}"
    printf '%s\n' "$LAST_ROUND_JSON" >>"$ROUNDS_LOG"
    fail_round evidence "默认任务要求创建/修改 soak.txt，但本轮结束时 $WS/soak.txt 不存在或为空"
  fi

  WS_BYTES=$(workspace_bytes)
  LAST_ROUND_JSON="{\"round\":$i,\"ok\":true,\"run\":$run_json,\"verify\":$verify_json,\"durationMs\":$((RUN_MS + VERIFY_MS)),\"workspaceBytes\":$WS_BYTES,\"writeCalls\":$WRITE_CALLS,\"shellCalls\":$SHELL_CALLS,\"tokens\":${run_tokens:-0}}"
  printf '%s\n' "$LAST_ROUND_JSON" >>"$ROUNDS_LOG"
  state running

  printf 'round=%s status=ok durationMs=%s runMs=%s verifyMs=%s journalEvents=%s(+%s) tools={write:%s,shell:%s,read:%s} tokens=%s totalTokens=%s workspaceBytes=%s\n' \
    "$i" "$((RUN_MS + VERIFY_MS))" "$RUN_MS" "$VERIFY_MS" "${verify_events:-0}" "$EVENTS_DELTA" \
    "$WRITE_CALLS" "$SHELL_CALLS" "${_rd:-0}" "${run_tokens:-0}" "$TOKENS_TOTAL" "$WS_BYTES" |
    tee -a "$LOG"

  if [ "$i" -lt "$ROUNDS" ] && [ "$INTERVAL" -gt 0 ]; then
    sleep "$INTERVAL"
  fi
done

# for ((...)) 结束后 i 会多加 1；state 的 rounds 记的是已完成轮数。
i=$ROUNDS

# 与 soak_long.sh 相同的收尾检查：不把「活着」当「健康」。
if command -v pgrep >/dev/null 2>&1 && pgrep -f "ui run --workspace $WS" >/dev/null 2>&1; then
  STATE_LAST_ERROR="仍有 ui run --workspace $WS 进程残留"
  state failed
  printf 'FAIL: 仍有 ui run --workspace %s 进程\n' "$WS" >&2
  pgrep -af "ui run --workspace $WS" >&2 || true
  exit 1
fi

state passed
printf 'PASS soak-real rounds=%s tokens≈%s workspace=%s log=%s state=%s commit=%s model=%s\n' \
  "$ROUNDS" "$TOKENS_TOTAL" "$WS" "$LOG" "$STATE" "$COMMIT" "$MODEL_LABEL" |
  tee -a "$LOG"
