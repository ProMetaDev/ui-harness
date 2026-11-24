#!/usr/bin/env bash
# 真实付费 provider 长跑入口：tools/soak_real.sh 的付费包装器。
#
# 为什么还要一层：soak_real.sh 只需要「UI_MODEL_CMD 非空 + UI_SOAK_ALLOW_COST=1」，
# 它不知道也不应该知道你的 key 从哪来、跑的是哪个模型、这一跑最多能花多少。
# 这个脚本把「付费」变成四道硬闸：凭据、模型、成本开关、人工确认。
# 任何一道缺失都 SKIP（exit 0，不产生任何模型调用）；配置值非法则 exit 2，
# 同样不调用。默认直接跑一定是 SKIP，避免 CI/手滑把账单打开。
#
# 它不复制 soak_real.sh 的轮次 / verify / state 逻辑：只准备环境并设置
# UI_SOAK_* / UI_MODEL_CMD，然后调用 tools/soak_real.sh；跑完把
# commit/model/tokens/耗时/state 汇总成 paid-report.json。真实轮次、工具证据、
# journal 链仍以 soak_real.sh 的 state/日志为准。
#
# 可用环境变量：
#   必填（缺一即 SKIP）：
#     UI_PAID_API_KEY 或 OPENAI_API_KEY   付费凭据（只通过环境变量传给适配器）
#     UI_PAID_MODEL                       模型名（不猜默认值）
#     UI_SOAK_ALLOW_COST=1                成本闸
#     UI_SOAK_YES=1                       非交互确认；交互终端则要求输入 yes
#
#   适配器：
#     UI_PAID_MODEL_CMD                   覆盖默认的 "node tools/chat_compat.mjs"；
#                                         命令内容不会回显/写进 report（防止夹带 key）
#     UI_PAID_BASE_URL / OPENAI_BASE_URL / UI_CHAT_BASE
#                                         OpenAI 兼容端点；会脱敏后打印。
#                                         默认适配器读 UI_CHAT_BASE/UI_CHAT_KEY/UI_MODEL，
#                                         也可回落到 secrets/astra.json（该目录被忽略）。
#     UI_PAID_USD_PER_MTOK                每百万 token 的美元价；只有设置它，ui 的
#                                         --max-usd 才会真正中止（内核不猜价格）。
#
#   预算/轮次（都有保守默认值，会写进 report）：
#     UI_SOAK_ROUNDS（默认 2）、UI_SOAK_INTERVAL（10）、
#     UI_PAID_RUN_TIMEOUT 或 UI_SOAK_RUN_TIMEOUT（默认 900 秒）、
#     UI_SOAK_MAX_TOKENS（200000）、UI_SOAK_MAX_CALLS（60）、
#     UI_SOAK_MAX_USD（0.50，需配合 UI_PAID_USD_PER_MTOK 才是硬中止）
#
#   产物路径（默认都在 /tmp，不碰仓库）：
#     UI_SOAK_WS（/tmp/ui-paid-soak）、UI_SOAK_LOG（/tmp/ui-paid-soak.log）、
#     UI_SOAK_STATE（/tmp/ui-paid-soak-state.json）、
#     UI_SOAK_ROUNDS_LOG（$STATE.rounds.jsonl）、
#     UI_PAID_REPORT（/tmp/ui-paid-soak-report.json）
#
# 退出码：0=passed 或 skipped；1=soak_real 的某一轮失败；2=配置错误（或
#         soak_real 自身以 2 提前退出）；3=报告里检测到 key（并用 [REDACTED]
#         修掉了）；130=被中断。
set -euo pipefail

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UI_ROOT="$(cd "$_here/.." && pwd)"
cd "$UI_ROOT"

# ── 基础工具（只做字符串/JSON 转义，不涉及 soak 判定逻辑） ───────────────
json_escape() {
  local s=${1-}
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\r'/\\r}
  s=${s//$'\t'/\\t}
  printf '%s' "$s"
}

build_skip_json() {
  local first=1 r
  if [ "${#SKIP_REASONS[@]}" -eq 0 ]; then
    printf 'null'
    return 0
  fi
  printf '['
  for r in "${SKIP_REASONS[@]}"; do
    if [ "$first" = 1 ]; then first=0; else printf ','; fi
    printf '"%s"' "$(json_escape "$r")"
  done
  printf ']'
}

is_uint() {
  case "${1:-}" in
    ''|*[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

is_pos_int() {
  is_uint "${1:-}" && [ "${1:-0}" -ge 1 ]
}

is_decimal() {
  case "${1:-}" in
    ''|*[!0-9.]*) return 1 ;;
    *.*.*|.*|*.) return 1 ;;
    *) return 0 ;;
  esac
}

# base URL 脱敏：去掉 userinfo / query / fragment，只留 scheme://host[:port]/path；
# 如果 key 被误写进 path，也替换成 [REDACTED]（key 只通过环境变量传入 python）。
redact_base() {
  local raw=${1-} out
  if [ -z "$raw" ]; then
    printf '(未设置；由适配器配置决定)'
    return 0
  fi
  out=$(PAID_REDACT_URL="$raw" PAID_REDACT_KEY="${KEY:-}" python3 - 2>/dev/null <<'PY'
import os
import urllib.parse

raw = os.environ.get("PAID_REDACT_URL", "").strip()
key = os.environ.get("PAID_REDACT_KEY", "")
try:
    p = urllib.parse.urlsplit(raw)
    if p.scheme:
        scheme = p.scheme + "://"
    else:
        p = urllib.parse.urlsplit("//" + raw)
        scheme = ""
    host = p.hostname or ""
    if not host:
        out = "[unparseable base URL]"
    else:
        try:
            port = (":" + str(p.port)) if p.port else ""
        except ValueError:
            port = ""
        path = p.path.rstrip("/") if p.path and p.path != "/" else ""
        out = f"{scheme}{host}{port}{path}"
except Exception:
    out = "[unparseable base URL]"
if key:
    out = out.replace(key, "[REDACTED]")
print(out)
PY
) || out=""
  [ -n "$out" ] || out='[unparseable base URL]'
  printf '%s' "$out"
}

# ── 闸门输入 ─────────────────────────────────────────────────────────────
KEY="${UI_PAID_API_KEY:-}"
KEY_SOURCE="UI_PAID_API_KEY"
if [ -z "$KEY" ]; then
  KEY="${OPENAI_API_KEY:-}"
  KEY_SOURCE="OPENAI_API_KEY"
fi
if [ -z "$KEY" ]; then
  KEY_SOURCE=""
fi

MODEL="${UI_PAID_MODEL:-}"
BASE_RAW="${UI_PAID_BASE_URL:-${OPENAI_BASE_URL:-${UI_CHAT_BASE:-}}}"
BASE_DISPLAY="$(redact_base "$BASE_RAW")"

WS="${UI_SOAK_WS:-${TMPDIR:-/tmp}/ui-paid-soak}"
LOG="${UI_SOAK_LOG:-${TMPDIR:-/tmp}/ui-paid-soak.log}"
STATE="${UI_SOAK_STATE:-${TMPDIR:-/tmp}/ui-paid-soak-state.json}"
ROUNDS_LOG="${UI_SOAK_ROUNDS_LOG:-$STATE.rounds.jsonl}"
REPORT="${UI_PAID_REPORT:-${TMPDIR:-/tmp}/ui-paid-soak-report.json}"

START_S=$(date +%s)
STARTED_AT="$START_S"
END_S="$START_S"
WALL_MS=0
SKIP_JSON=null
SKIP_REASONS=()
REPORT_DONE=0
RUN_STARTED=0
INTERRUPTED=0
SANITIZED=0
CHILD_PID=0
USD_ENFORCED=0

# ── 默认预算/轮次（先算出来，plan 和 report 用同一组值） ────────────────
ROUNDS="${UI_SOAK_ROUNDS:-2}"
INTERVAL="${UI_SOAK_INTERVAL:-10}"
RUN_TIMEOUT="${UI_PAID_RUN_TIMEOUT:-${UI_SOAK_RUN_TIMEOUT:-900}}"
MAX_TOKENS="${UI_SOAK_MAX_TOKENS:-200000}"
MAX_CALLS="${UI_SOAK_MAX_CALLS:-60}"
MAX_USD="${UI_SOAK_MAX_USD:-0.50}"
USD_PER_MTOK="${UI_PAID_USD_PER_MTOK:-${UI_SOAK_USD_PER_MTOK:-}}"

if [ -n "${UI_PAID_MODEL_CMD:-}" ]; then
  MODEL_CMD="$UI_PAID_MODEL_CMD"
  PROVIDER_DESC="custom（UI_PAID_MODEL_CMD；命令内容不回显，避免命令里夹带凭据）"
else
  MODEL_CMD="node tools/chat_compat.mjs"
  PROVIDER_DESC="node tools/chat_compat.mjs（默认 OpenAI 兼容适配器）"
fi

# ── report：汇总 soak_real 的 state / rounds.jsonl ───────────────────────
write_report() {
  local phase="$1" rc="$2" reason="${3:-}" use_state="${4:-0}"
  local tmp
  mkdir -p "$(dirname "$REPORT")" 2>/dev/null || {
    printf 'WARN: 无法创建 report 目录: %s\n' "$(dirname "$REPORT")" >&2
    return 1
  }
  END_S=$(date +%s)
  WALL_MS=$(( (END_S - START_S) * 1000 ))

  export PAID_RPT_PHASE="$phase"
  export PAID_RPT_EXIT="$rc"
  export PAID_RPT_REASON="$reason"
  export PAID_RPT_USE_STATE="$use_state"
  export PAID_RPT_STARTED="$STARTED_AT"
  export PAID_RPT_FINISHED="$END_S"
  export PAID_RPT_WALL_MS="$WALL_MS"
  export PAID_RPT_MODEL="$MODEL"
  export PAID_RPT_BASE="$BASE_DISPLAY"
  export PAID_RPT_PROVIDER="$PROVIDER_DESC"
  export PAID_RPT_KEY_SOURCE="$KEY_SOURCE"
  export PAID_RPT_ROUNDS="$ROUNDS"
  export PAID_RPT_MAX_TOKENS="$MAX_TOKENS"
  export PAID_RPT_MAX_CALLS="$MAX_CALLS"
  export PAID_RPT_MAX_USD="$MAX_USD"
  export PAID_RPT_USD_PER_MTOK="$USD_PER_MTOK"
  export PAID_RPT_USD_ENFORCED="$USD_ENFORCED"
  export PAID_RPT_SANITIZED="$SANITIZED"
  export PAID_RPT_SKIP_JSON="${SKIP_JSON:-null}"
  export PAID_STATE_PATH="$STATE"
  export PAID_ROUNDS_PATH="$ROUNDS_LOG"
  export PAID_LOG_PATH="$LOG"
  export PAID_WS_PATH="$WS"

  tmp="$REPORT.tmp.$$"
  if ! python3 - "$tmp" <<'PY'
import json
import os
import sys
from datetime import datetime, timezone


def env(name, default=None):
    v = os.environ.get(name)
    if v is None or v == "":
        return default
    return v


def as_int(name, default=None):
    v = env(name)
    if v is None:
        return default
    try:
        return int(v)
    except (TypeError, ValueError):
        return default


def as_float(name, default=None):
    v = env(name)
    if v is None:
        return default
    try:
        return float(v)
    except (TypeError, ValueError):
        return default


def read_json(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None


def number(v):
    if isinstance(v, bool):
        return None
    return v if isinstance(v, (int, float)) else None


phase = env("PAID_RPT_PHASE", "failed")
exit_code = as_int("PAID_RPT_EXIT", 1)
reason = env("PAID_RPT_REASON", "")
use_state = env("PAID_RPT_USE_STATE", "0") == "1"
state_path = env("PAID_STATE_PATH", "")
rounds_path = env("PAID_ROUNDS_PATH", "")
log_path = env("PAID_LOG_PATH", "")
ws_path = env("PAID_WS_PATH", "")

state = read_json(state_path) if use_state and state_path else None
if not isinstance(state, dict):
    state = None
# 防止把上一轮的 state 当成本轮结果：state.startedAt 早于本次 wrapper 启动就忽略。
started_at = as_int("PAID_RPT_STARTED")
if state and started_at is not None:
    state_started = number(state.get("startedAt"))
    if state_started is not None and state_started < started_at:
        state = None

round_rows = []
if use_state and rounds_path:
    try:
        with open(rounds_path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    row = json.loads(line)
                except Exception:
                    continue
                if isinstance(row, dict):
                    round_rows.append(row)
    except OSError:
        pass

per_round = []
for row in round_rows:
    run = row.get("run") if isinstance(row.get("run"), dict) else {}
    verify = row.get("verify") if isinstance(row.get("verify"), dict) else {}
    per_round.append(
        {
            "round": row.get("round"),
            "ok": row.get("ok"),
            "durationMs": number(row.get("durationMs")),
            "tokens": number(row.get("tokens")),
            "runMs": number(run.get("ms")),
            "verifyMs": number(verify.get("ms")),
            "runRc": number(run.get("rc")),
            "verifyRc": number(verify.get("rc")),
            "writeCalls": number(row.get("writeCalls")),
            "shellCalls": number(row.get("shellCalls")),
            "runReason": run.get("reason") if isinstance(run.get("reason"), str) else None,
        }
    )

state_phase = state.get("phase") if state else None
state_tokens = number(state.get("tokensTotal")) if state else None
round_tokens = sum(r["tokens"] for r in per_round if r["tokens"] is not None)
if state_tokens is None:
    tokens_total = round_tokens
    tokens_source = "rounds jsonl" if per_round else "not-run"
elif round_tokens > state_tokens:
    tokens_total = round_tokens
    tokens_source = "rounds jsonl（state.tokensTotal 未更新）"
else:
    tokens_total = int(state_tokens)
    tokens_source = "state.tokensTotal"

rounds_ms = sum(r["durationMs"] for r in per_round if r["durationMs"] is not None)
run_ms_total = sum(r["runMs"] for r in per_round if r["runMs"] is not None)
verify_ms_total = sum(r["verifyMs"] for r in per_round if r["verifyMs"] is not None)
wall_ms = as_int("PAID_RPT_WALL_MS", 0) or 0
state_wall_ms = None
if state:
    started = number(state.get("startedAt"))
    finished = number(state.get("finishedAt"))
    if started is not None and finished is not None:
        state_wall_ms = int(max(0, finished - started) * 1000)
duration_ms = wall_ms or state_wall_ms or rounds_ms

planned = as_int("PAID_RPT_ROUNDS", state.get("roundsPlanned") if state else 0) or 0
completed = state.get("rounds") if state and isinstance(state.get("rounds"), int) else 0
completed = max(completed, len(per_round))
ok_rounds = sum(1 for r in per_round if r.get("ok") is True)

max_tokens = as_int("PAID_RPT_MAX_TOKENS")
max_calls = as_int("PAID_RPT_MAX_CALLS")
max_usd = as_float("PAID_RPT_MAX_USD")
usd_per_mtok = as_float("PAID_RPT_USD_PER_MTOK")
usd_enforced = env("PAID_RPT_USD_ENFORCED", "0") == "1"


def total(v):
    if v is None or planned <= 0:
        return None
    if isinstance(v, float):
        return round(v * planned, 6)
    return v * planned


budget = {
    "applied": use_state,
    "perRound": {
        "maxTokens": max_tokens,
        "maxCalls": max_calls,
        "maxUsd": max_usd,
        "usdPerMtok": usd_per_mtok,
        "usdHardCap": usd_enforced,
    },
    "worstCaseTotal": {
        "maxTokens": total(max_tokens),
        "maxCalls": total(max_calls),
        "maxUsd": total(max_usd),
    },
    "note": (
        "预算由 ui run 按轮重置；单轮触顶即中止该轮。"
        if usd_enforced
        else "未设置 UI_PAID_USD_PER_MTOK：--max-usd 不产生金额中止；硬上限是 tokens/calls/墙钟。"
    ),
}

skip_reasons = None
skip_raw = env("PAID_RPT_SKIP_JSON", "null")
if skip_raw and skip_raw != "null":
    try:
        parsed = json.loads(skip_raw)
        skip_reasons = parsed if isinstance(parsed, list) else [str(parsed)]
    except Exception:
        skip_reasons = [skip_raw]

if use_state:
    state_info = {
        "path": state_path or None,
        "present": state is not None,
        "phase": state_phase,
        "runId": state.get("runId") if state else None,
        "startedAt": state.get("startedAt") if state else None,
        "finishedAt": state.get("finishedAt") if state else None,
        "rounds": state.get("rounds") if state else None,
        "roundsPlanned": state.get("roundsPlanned") if state else None,
        "tokensTotal": state_tokens,
        "lastError": state.get("lastError") if state else None,
        "lastRound": state.get("lastRound") if state else None,
        "log": state.get("log") if state else None,
        "roundsLog": state.get("roundsLog") if state else None,
        "workspace": state.get("workspace") if state else None,
    }
else:
    state_info = {
        "path": state_path or None,
        "used": False,
        "note": "未调用 tools/soak_real.sh（SKIP 或配置错误），不读取任何旧 state",
    }

if reason:
    error = reason
elif state and state.get("lastError"):
    error = state.get("lastError")
else:
    error = None

reason_l = (reason or "").lower()
if phase == "skipped":
    error_kind = "skip"
elif reason_l.startswith("config:"):
    error_kind = "config"
elif "interrupt" in reason_l:
    error_kind = "interrupted"
else:
    error_kind = "run"

commit = None
if state:
    c = state.get("commit")
    if isinstance(c, str) and c:
        commit = c
model = env("PAID_RPT_MODEL") or (state.get("modelLabel") if state else None) or None
base = env("PAID_RPT_BASE") or None
provider = env("PAID_RPT_PROVIDER") or None
key_source = env("PAID_RPT_KEY_SOURCE") or None

report = {
    "schemaVersion": 1,
    "kind": "paid-soak",
    "phase": phase,
    "exitCode": exit_code,
    "errorKind": error_kind,
    "error": error,
    "generatedAt": datetime.now(timezone.utc).isoformat(),
    "startedAt": as_int("PAID_RPT_STARTED"),
    "finishedAt": as_int("PAID_RPT_FINISHED"),
    "durationMs": duration_ms,
    "commit": commit,
    "model": model,
    "baseUrlRedacted": base,
    "provider": provider,
    "keySource": key_source,
    "rounds": {
        "planned": planned,
        "completed": completed,
        "ok": ok_rounds,
        "perRound": per_round,
    },
    "budget": budget,
    "tokens": {
        "total": int(tokens_total),
        "perRound": [r["tokens"] for r in per_round],
        "source": tokens_source,
        "note": "约 tokens，来自 ui run 输出/state；可能是估算值，账单以 provider 为准。",
    },
    "timing": {
        "wallMs": wall_ms,
        "stateWallMs": state_wall_ms,
        "roundsMs": rounds_ms,
        "runMsTotal": run_ms_total,
        "verifyMsTotal": verify_ms_total,
        "perRoundMs": [r["durationMs"] for r in per_round],
    },
    "state": state_info,
    "skipReasons": skip_reasons,
    "sanitized": env("PAID_RPT_SANITIZED", "0") == "1",
    "paths": {"workspace": ws_path, "log": log_path, "roundsLog": rounds_path},
    "notes": [
        "paid-report.json 是本地运行记录，不是 provider 账单或收据。",
        "不包含 API key；key 只通过环境变量传给适配器，报告里只有来源变量名。",
        "真实调用次数/费用以 provider 账单为准；本文件不伪造调用证据。",
    ],
}

out_path = sys.argv[1]
fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w", encoding="utf-8") as f:
    json.dump(report, f, ensure_ascii=False, indent=2)
    f.write("\n")
PY
  then
    rm -f "$tmp"
    return 1
  fi
  mv -f "$tmp" "$REPORT"
  chmod 600 "$REPORT" 2>/dev/null || true
  REPORT_DONE=1
  return 0
}

# key 只可能因为自定义适配器把环境变量打到 stderr 而混进日志；发现就就地抹掉。
# 这里不改 journal（改 journal 会破坏哈希链），只处理 wrapper 的 log/state/report。
scan_one_file() {
  local f="$1" out
  [ -f "$f" ] || return 0
  [ -n "$KEY" ] || return 0
  [ "${#KEY}" -ge 8 ] || return 0
  out=$(PAID_SCAN_KEY="$KEY" python3 - "$f" <<'PY'
import os
import sys

path = sys.argv[1]
key = os.environ.get("PAID_SCAN_KEY", "")
try:
    with open(path, "rb") as f:
        data = f.read()
except OSError:
    sys.exit(0)
needle = key.encode()
if needle and needle in data:
    with open(path, "wb") as f:
        f.write(data.replace(needle, b"[REDACTED]"))
    print("found")
PY
) || return 0
  if [ "$out" = "found" ]; then
    printf 'WARN: %s 里出现了 key，已替换为 [REDACTED]\n' "$f" >&2
    return 1
  fi
  return 0
}

scrub_artifacts() {
  local f
  for f in "$LOG" "$STATE" "$ROUNDS_LOG"; do
    if [ -f "$f" ] && ! scan_one_file "$f"; then
      SANITIZED=1
    fi
  done
}

skip_now() {
  SKIP_JSON="$(build_skip_json)"
  printf 'SKIP paid-soak：未进入真实计费运行（没有产生任何模型调用）\n'
  local r
  for r in "${SKIP_REASONS[@]}"; do
    printf '  - %s\n' "$r"
  done
  printf '真正开跑需要同时满足：\n'
  printf '  UI_PAID_API_KEY（或 OPENAI_API_KEY）+ UI_PAID_MODEL\n'
  printf '  UI_SOAK_ALLOW_COST=1\n'
  printf '  UI_SOAK_YES=1（或在交互终端输入 yes）\n'
  printf '说明：docs/53-paid-soak.md\n'
  if ! write_report "skipped" 0 "" 0; then
    printf 'WARN: skipped report 未写成: %s\n' "$REPORT" >&2
  fi
  exit 0
}

config_fail() {
  local why="$1"
  printf 'FAIL paid-soak: %s\n' "$why" >&2
  printf '（配置错误，未产生任何模型调用；修好后重跑）\n' >&2
  if ! write_report "failed" 2 "config: $why" 0; then
    printf 'WARN: failed report 未写成: %s\n' "$REPORT" >&2
  fi
  exit 2
}

on_signal() {
  INTERRUPTED=1
  if [ "$CHILD_PID" -gt 0 ]; then
    kill -TERM "$CHILD_PID" 2>/dev/null || true
  fi
}

on_exit() {
  local rc=$?
  if [ "${REPORT_DONE:-0}" = "1" ]; then
    return 0
  fi
  if [ "${RUN_STARTED:-0}" = "1" ]; then
    scrub_artifacts || true
    write_report "failed" "$rc" "wrapper exited unexpectedly (rc=$rc)" 1 || true
  fi
}
trap on_exit EXIT
trap 'on_signal' INT
trap 'on_signal' TERM

# ── 四道闸：凭据 / 模型 / 成本开关 / 人工确认 ────────────────────────────
if [ -z "$KEY" ]; then
  SKIP_REASONS+=("缺少 UI_PAID_API_KEY 或 OPENAI_API_KEY：没有付费凭据")
fi
if [ -z "$MODEL" ]; then
  SKIP_REASONS+=("缺少 UI_PAID_MODEL：不猜模型，避免跑错价格档")
fi
if [ "${UI_SOAK_ALLOW_COST:-}" != "1" ]; then
  SKIP_REASONS+=("UI_SOAK_ALLOW_COST != 1：成本闸未显式打开")
fi

if [ "${#SKIP_REASONS[@]}" -gt 0 ]; then
  skip_now
fi

if [ -n "${UI_MODEL_CMD:-}" ] && [ -z "${UI_PAID_MODEL_CMD:-}" ]; then
  printf 'WARN: 忽略环境里的 UI_MODEL_CMD；付费入口默认用 node tools/chat_compat.mjs，覆盖请设 UI_PAID_MODEL_CMD。\n' >&2
fi

# ── 配置校验（非法配置 exit 2，且发生在任何模型调用之前） ───────────────
is_pos_int "$ROUNDS" || config_fail "UI_SOAK_ROUNDS 必须是正整数: $ROUNDS"
is_uint "$INTERVAL" || config_fail "UI_SOAK_INTERVAL 必须是非负整数: $INTERVAL"
is_uint "$RUN_TIMEOUT" || config_fail "UI_PAID_RUN_TIMEOUT/UI_SOAK_RUN_TIMEOUT 必须是非负整数: $RUN_TIMEOUT"
is_pos_int "$MAX_TOKENS" || config_fail "UI_SOAK_MAX_TOKENS 必须是正整数: $MAX_TOKENS"
is_pos_int "$MAX_CALLS" || config_fail "UI_SOAK_MAX_CALLS 必须是正整数: $MAX_CALLS"
is_decimal "$MAX_USD" || config_fail "UI_SOAK_MAX_USD 必须是非负数字: $MAX_USD"
if [ -n "$USD_PER_MTOK" ]; then
  is_decimal "$USD_PER_MTOK" || config_fail "UI_PAID_USD_PER_MTOK 必须是非负数字: $USD_PER_MTOK"
fi

# 去掉前导 0，避免 bash 算术把 08 当八进制（ui 接受这些值，这里只做规范化）。
ROUNDS=$((10#$ROUNDS))
INTERVAL=$((10#$INTERVAL))
RUN_TIMEOUT=$((10#$RUN_TIMEOUT))
MAX_TOKENS=$((10#$MAX_TOKENS))
MAX_CALLS=$((10#$MAX_CALLS))

USD_ENFORCED=0
if [ -n "$USD_PER_MTOK" ] && awk -v u="$USD_PER_MTOK" 'BEGIN { exit !(u > 0) }'; then
  USD_ENFORCED=1
fi

# 默认适配器是 node 脚本：没有 node 就明确失败，不进入确认/计费。
if [ -z "${UI_PAID_MODEL_CMD:-}" ]; then
  if ! command -v node >/dev/null 2>&1; then
    config_fail "找不到 node：默认适配器是 node tools/chat_compat.mjs；可装 node，或用 UI_PAID_MODEL_CMD 覆盖"
  fi
  if [ ! -f "$UI_ROOT/tools/chat_compat.mjs" ]; then
    config_fail "缺少 tools/chat_compat.mjs：默认适配器不存在"
  fi
fi

# ── 成本计划（先打印，再确认，再动手） ───────────────────────────────────
TOKENS_CAP_TOTAL=$((ROUNDS * MAX_TOKENS))
CALLS_CAP_TOTAL=$((ROUNDS * MAX_CALLS))
USD_CAP_TOTAL=$(awk -v r="$ROUNDS" -v u="$MAX_USD" 'BEGIN { printf "%.2f", r * u }')
if [ "$RUN_TIMEOUT" -gt 0 ]; then
  WALL_CAP=$((ROUNDS * (RUN_TIMEOUT + 60) + (ROUNDS - 1) * INTERVAL))
  WALL_DESC="$WALL_CAP 秒（每轮 run 超时 ${RUN_TIMEOUT}s + verify 余量 + 间隔）"
else
  WALL_DESC="无上界（UI_SOAK_RUN_TIMEOUT=0），卡住不会自动重试"
fi

printf 'PAID-SOAK 计划（真实计费）\n'
printf '  模型: %s\n' "$MODEL"
printf '  base URL（脱敏）: %s\n' "$BASE_DISPLAY"
printf '  provider: %s\n' "$PROVIDER_DESC"
printf '  commit: 启动时 HEAD，由 tools/soak_real.sh 写入 state/report\n'
printf '  轮数: %s（间隔 %ss，单轮 ui run 超时 %ss）\n' "$ROUNDS" "$INTERVAL" "$RUN_TIMEOUT"
printf '  预计模型调用下限: %s 次（%s 轮 × 2：一次工具调用 + 一次收尾）；默认任务常见 %s 次（%s 轮 × 3：写、读、收尾）；模型走弯路/适配器重试会更多。\n' \
  "$((ROUNDS * 2))" "$ROUNDS" "$((ROUNDS * 3))" "$ROUNDS"
printf '  预算上限: 每轮 max_tokens=%s, max_calls=%s, max_usd=%s；最坏合计 tokens=%s, calls=%s, usd=%s。\n' \
  "$MAX_TOKENS" "$MAX_CALLS" "$MAX_USD" "$TOKENS_CAP_TOTAL" "$CALLS_CAP_TOTAL" "$USD_CAP_TOTAL"
if [ "$USD_ENFORCED" = 1 ]; then
  printf '  USD 硬中止: 已启用（--usd-per-mtok=%s）；每轮达到 $%s 即中止该轮。\n' "$USD_PER_MTOK" "$MAX_USD"
else
  printf '  USD 硬中止: 未启用（未设置 UI_PAID_USD_PER_MTOK）。--max-usd 只是名义值；真正硬中止的是 tokens/calls/墙钟。\n'
  printf '  要按金额中止：设置 UI_PAID_USD_PER_MTOK=<provider 每百万 token 的美元价>。\n'
fi
printf '  预计墙钟: 最坏约 %s。\n' "$WALL_DESC"
printf '  计费风险: 真实端点按 token 计费；触顶只中止后续调用，已发生的调用仍会计费；适配器内部可能重试（chat_compat 默认 3 次）；tokens 统计不等于账单。\n'

# ── 人工确认 ─────────────────────────────────────────────────────────────
if [ "${UI_SOAK_YES:-}" = "1" ]; then
  printf '  确认: UI_SOAK_YES=1（跳过交互确认）\n'
elif [ -t 0 ] && [ -t 1 ]; then
  printf '\n这是真实计费运行。确认请输入 yes（其他输入取消）: '
  answer=""
  read -r answer || answer=""
  if [ "$answer" != "yes" ]; then
    SKIP_REASONS+=("人工确认未通过（没有输入 yes）")
    skip_now
  fi
  printf '  确认: 已人工输入 yes\n'
else
  SKIP_REASONS+=("非交互环境且未设置 UI_SOAK_YES=1：拒绝在没有人工确认时开始计费")
  skip_now
fi

# ── 交给 soak_real.sh；只准备环境，不复制它的轮次/verify/state 逻辑 ─────
export UI_SOAK_ALLOW_COST=1
export UI_SOAK_ROUNDS="$ROUNDS"
export UI_SOAK_INTERVAL="$INTERVAL"
export UI_SOAK_RUN_TIMEOUT="$RUN_TIMEOUT"
export UI_SOAK_REQUIRE_TOOL=1
export UI_SOAK_MAX_TOKENS="$MAX_TOKENS"
export UI_SOAK_MAX_CALLS="$MAX_CALLS"
export UI_SOAK_MAX_USD="$MAX_USD"
if [ -n "$USD_PER_MTOK" ]; then
  export UI_SOAK_USD_PER_MTOK="$USD_PER_MTOK"
else
  unset UI_SOAK_USD_PER_MTOK 2>/dev/null || true
fi
export UI_SOAK_WS="$WS"
export UI_SOAK_LOG="$LOG"
export UI_SOAK_STATE="$STATE"
export UI_SOAK_ROUNDS_LOG="$ROUNDS_LOG"
export UI_SOAK_MODEL_LABEL="$MODEL"
export UI_MODEL_CMD="$MODEL_CMD"
export UI_CHAT_KEY="$KEY"
if [ -n "$BASE_RAW" ]; then
  export UI_CHAT_BASE="$BASE_RAW"
fi
export UI_MODEL="$MODEL"

mkdir -p "$(dirname "$REPORT")" "$(dirname "$STATE")" "$(dirname "$LOG")" "$(dirname "$ROUNDS_LOG")" 2>/dev/null || true
# 只清自己的 report：state/rounds 由 soak_real 覆盖，且 report 会按 state.startedAt
# 拒绝把上一轮 state 当成本次结果（不删用户指定路径上的文件）。
rm -f "$REPORT" 2>/dev/null || true

printf '\n启动 tools/soak_real.sh（真实 provider；失败即停，不自动重试）...\n'
RUN_STARTED=1
bash "$UI_ROOT/tools/soak_real.sh" &
CHILD_PID=$!
set +e
wait "$CHILD_PID"
rc=$?
set -e
CHILD_PID=0

# 以 soak_real 写的 state 为准；包装器自己的退出码只说明它有没有正常退出。
STATE_PHASE=""
STATE_ERROR=""
if [ -f "$STATE" ]; then
  STATE_INFO=$(PAID_STATE_PATH="$STATE" PAID_STATE_STARTED="$START_S" python3 - 2>/dev/null <<'PY'
import json
import os
import sys

try:
    with open(os.environ["PAID_STATE_PATH"], encoding="utf-8") as f:
        s = json.load(f)
    started = int(os.environ.get("PAID_STATE_STARTED", "0") or 0)
    state_started = s.get("startedAt")
    if isinstance(state_started, int) and started and state_started < started:
        print("")
        print("")
        sys.exit(0)
    print(s.get("phase") or "")
    print(s.get("lastError") or "")
except Exception:
    print("")
    print("")
PY
) || STATE_INFO=$'\n'
  { IFS= read -r STATE_PHASE; IFS= read -r STATE_ERROR; } <<<"$STATE_INFO" || true
  STATE_PHASE=${STATE_PHASE:-}
  STATE_ERROR=${STATE_ERROR:-}
fi

PHASE="failed"
reason=""
if [ "$INTERRUPTED" = 1 ]; then
  reason="interrupted by signal"
elif [ -z "$STATE_PHASE" ]; then
  reason="soak_real exited rc=$rc but state is missing"
elif [ "$STATE_PHASE" = "failed" ]; then
  reason="$STATE_ERROR"
  [ -n "$reason" ] || reason="soak_real exit=$rc"
elif [ "$STATE_PHASE" = "passed" ]; then
  PHASE="passed"
elif [ "$STATE_PHASE" = "skipped" ]; then
  PHASE="skipped"
else
  reason="state phase=$STATE_PHASE after soak_real exit=$rc（soak_real 没有走到收尾的 failed/passed）"
fi

# 退出码与 state 必须一致：state 说失败就不能以 0 退出；state 说通过就不能保留
# 一个非 0 退出码。skip 仍按协议 exit 0。
if [ "$PHASE" = "passed" ] && [ "$rc" -ne 0 ]; then
  PHASE="failed"
  reason="state=passed but soak_real exit=$rc"
fi
if [ "$PHASE" = "failed" ] && [ "$rc" -eq 0 ]; then
  rc=1
  [ -n "$reason" ] || reason="state=$STATE_PHASE but exit=0"
fi
if [ "$PHASE" = "skipped" ]; then
  rc=0
fi
if [ "$INTERRUPTED" = 1 ]; then
  PHASE="failed"
  if [ "$rc" -eq 0 ] || [ "$rc" -ge 128 ]; then
    rc=130
  fi
fi

# 先擦掉日志/state 里可能出现的 key，再写 report（report 本身只含来源变量名）。
scrub_artifacts || true

if ! write_report "$PHASE" "$rc" "$reason" 1; then
  printf 'WARN: report 未写成: %s\n' "$REPORT" >&2
  [ "$rc" -eq 0 ] && rc=2
fi

# 最后一道保险：report 里不能有 key。
if ! scan_one_file "$REPORT"; then
  SANITIZED=1
  # key 被替换后 JSON 可能不再合法；既然它不应该出现，直接重写成最小失败报告。
  PAID_REPORT_PATH="$REPORT" python3 - 2>/dev/null <<'PY' || true
import json
import os

path = os.environ["PAID_REPORT_PATH"]
report = {
    "schemaVersion": 1,
    "kind": "paid-soak",
    "phase": "failed",
    "exitCode": 3,
    "errorKind": "run",
    "error": "检测到 key 出现在 paid-report.json，已删除原报告；请检查适配器是否把环境变量打到了输出。",
    "sanitized": True,
}
with open(path, "w", encoding="utf-8") as f:
    json.dump(report, f, ensure_ascii=False, indent=2)
    f.write("\n")
PY
  rc=3
fi

# 汇总一行（不打印 key/命令）。
summary=$(PAID_REPORT_PATH="$REPORT" python3 - 2>/dev/null <<'PY'
import json
import os

try:
    with open(os.environ["PAID_REPORT_PATH"], encoding="utf-8") as f:
        r = json.load(f)
    print(
        "phase=%s commit=%s model=%s tokens=%s durationMs=%s"
        % (
            r.get("phase"),
            r.get("commit") or "unknown",
            r.get("model") or "unknown",
            (r.get("tokens") or {}).get("total"),
            r.get("durationMs"),
        )
    )
except Exception:
    print("phase=unknown commit=unknown model=unknown tokens=0 durationMs=0")
PY
) || summary="phase=unknown commit=unknown model=unknown tokens=0 durationMs=0"

if [ "$rc" -eq 0 ]; then
  printf 'PASS paid-soak %s report=%s state=%s\n' "$summary" "$REPORT" "$STATE"
else
  printf 'FAIL paid-soak rc=%s %s report=%s state=%s\n' "$rc" "$summary" "$REPORT" "$STATE" >&2
fi
exit "$rc"
