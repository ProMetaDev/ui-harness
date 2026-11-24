#!/usr/bin/env bash
# 真实付费 provider 的固定 1 轮 smoke：tools/soak_paid.sh 的薄包装器。
#
# 定位：只回答「这个 commit + 这个模型 + 这个端点，能不能跑完 1 轮带工具的任务」。
# 它不复制 soak_paid.sh / soak_real.sh 的轮次、verify、state 逻辑：只做四道闸，
# 固定轮数与预算，然后调用 tools/soak_paid.sh；跑完把结果汇总成 smoke-report.json。
#
# 默认一定 SKIP（exit 0，不产生任何模型调用），避免 CI / shell profile 里的
# OPENAI_API_KEY 被顺手打开账单：
#   1. UI_PAID_API_KEY 或 OPENAI_API_KEY   付费凭据
#   2. UI_PAID_MODEL                       模型名（不猜默认值）
#   3. UI_SOAK_ALLOW_COST=1                成本闸
#   4. UI_SOAK_YES=1                       非交互确认（本入口不读交互输入）
#
# 与 soak_paid.sh 的区别：
#   - 轮数固定 1（忽略/覆盖 UI_SOAK_ROUNDS，不跑长跑）；
#   - 预算默认更小（max_usd 0.01 / max_tokens 30000 / max_calls 20 / run 300s）；
#   - 非交互环境必须显式 UI_SOAK_YES=1，不提供交互确认分支；
#   - 额外产出 smoke-report.json，字段固定为 commit/model/phase/tokens/cost/state/log。
#
# 可用环境变量（预算/路径，都有保守默认值）：
#   UI_SMOKE_MAX_USD / UI_SOAK_MAX_USD                  默认 0.01
#   UI_SMOKE_MAX_TOKENS / UI_SOAK_MAX_TOKENS            默认 30000
#   UI_SMOKE_MAX_CALLS / UI_SOAK_MAX_CALLS              默认 20
#   UI_SMOKE_RUN_TIMEOUT / UI_SOAK_RUN_TIMEOUT /
#     UI_PAID_RUN_TIMEOUT                               默认 300 秒
#   UI_SMOKE_USD_PER_MTOK / UI_PAID_USD_PER_MTOK /
#     UI_SOAK_USD_PER_MTOK                              可选；设置后 --max-usd 才硬中止
#   UI_SMOKE_REPORT                                     默认 /tmp/ui-paid-smoke-report.json
#   UI_SMOKE_LOG                                        默认 /tmp/ui-paid-smoke.log
#   UI_SMOKE_STATE                                      默认 /tmp/ui-paid-smoke-state.json
#   UI_SMOKE_ROUNDS_LOG                                 默认 $STATE.rounds.jsonl
#   UI_SMOKE_WS                                         默认 /tmp/ui-paid-smoke-ws
#   UI_SMOKE_PAID_REPORT                                默认 /tmp/ui-paid-smoke-paid-report.json
#
# 适配器：UI_PAID_MODEL_CMD 原样透传给 soak_paid.sh（用于 scripted e2e）；
#         未设置时 soak_paid.sh 使用 node tools/chat_compat.mjs。
#
# 退出码：0=passed 或 skipped；1=soak_paid/soak_real 失败；2=配置错误；
#         3=smoke-report.json 里检测到 key（已替换成最小失败报告）；130=中断。
set -euo pipefail

_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UI_ROOT="$(cd "$_here/.." && pwd)"
cd "$UI_ROOT"

# ── 小型工具：JSON 转义、数值校验、当前 commit ─────────────────────────
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

is_positive_decimal() {
  is_decimal "${1:-}" && awk -v v="${1:-0}" 'BEGIN { exit !(v > 0) }'
}

# 与 soak_real.sh 相同的 worktree 兼容：WSL 里 .git 文件可能写着 X:/...，
# 直接 git 不认，映射到 /mnt/x/... 再试。只用于 report 的 commit 字段。
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

# ── 路径与汇总输入（先算出来，SKIP 也要写 report） ──────────────────────
REPORT="${UI_SMOKE_REPORT:-${TMPDIR:-/tmp}/ui-paid-smoke-report.json}"
LOG="${UI_SMOKE_LOG:-${TMPDIR:-/tmp}/ui-paid-smoke.log}"
STATE="${UI_SMOKE_STATE:-${TMPDIR:-/tmp}/ui-paid-smoke-state.json}"
ROUNDS_LOG="${UI_SMOKE_ROUNDS_LOG:-$STATE.rounds.jsonl}"
WS="${UI_SMOKE_WS:-${TMPDIR:-/tmp}/ui-paid-smoke-ws}"
PAID_REPORT="${UI_SMOKE_PAID_REPORT:-${TMPDIR:-/tmp}/ui-paid-smoke-paid-report.json}"

START_S=$(date +%s)
COMMIT_FALLBACK="$(git_commit)"
SKIP_REASONS=()

# SKIP / 配置错误也要写 report；先放保守默认值，闸门通过后再覆盖并校验。
MAX_USD=0.01
MAX_TOKENS=30000
MAX_CALLS=20
RUN_TIMEOUT=300
USD_PER_MTOK=""

# ── 四道闸：缺任意一道都只 SKIP，不进入 soak_paid.sh ────────────────────
KEY="${UI_PAID_API_KEY:-}"
if [ -z "$KEY" ]; then
  KEY="${OPENAI_API_KEY:-}"
fi
MODEL="${UI_PAID_MODEL:-}"

if [ -z "$KEY" ]; then
  SKIP_REASONS+=("缺少 UI_PAID_API_KEY 或 OPENAI_API_KEY：没有付费凭据")
fi
if [ -z "$MODEL" ]; then
  SKIP_REASONS+=("缺少 UI_PAID_MODEL：不猜模型，避免跑错价格档")
fi
if [ "${UI_SOAK_ALLOW_COST:-}" != "1" ]; then
  SKIP_REASONS+=("UI_SOAK_ALLOW_COST != 1：成本闸未显式打开")
fi
if [ "${UI_SOAK_YES:-}" != "1" ]; then
  SKIP_REASONS+=("UI_SOAK_YES != 1：非交互 smoke 必须显式确认，不假设没人看着也可以花")
fi

# ── report 写入：只输出白名单字段，不抄适配器命令 / 不抄 key ────────────
write_report() {
  local phase="$1" rc="$2" reason="${3:-}" use_paid="${4:-0}"
  local skip_json tmp
  skip_json="$(build_skip_json)"
  mkdir -p "$(dirname "$REPORT")" 2>/dev/null || {
    printf 'WARN: 无法创建 smoke-report 目录: %s\n' "$(dirname "$REPORT")" >&2
    return 1
  }

  export SMOKE_RPT_PHASE="$phase"
  export SMOKE_RPT_EXIT="$rc"
  export SMOKE_RPT_REASON="$reason"
  export SMOKE_RPT_USE_PAID="$use_paid"
  export SMOKE_RPT_STARTED="$START_S"
  export SMOKE_RPT_FINISHED="$(date +%s)"
  export SMOKE_RPT_COMMIT_FALLBACK="$COMMIT_FALLBACK"
  export SMOKE_RPT_MODEL_FALLBACK="$MODEL"
  export SMOKE_RPT_SKIP_JSON="$skip_json"
  export SMOKE_STATE_PATH="$STATE"
  export SMOKE_LOG_PATH="$LOG"
  export SMOKE_PAID_REPORT_PATH="$PAID_REPORT"
  export SMOKE_WS_PATH="$WS"
  export SMOKE_MAX_USD="$MAX_USD"
  export SMOKE_MAX_TOKENS="$MAX_TOKENS"
  export SMOKE_MAX_CALLS="$MAX_CALLS"
  export SMOKE_RUN_TIMEOUT="$RUN_TIMEOUT"
  export SMOKE_USD_PER_MTOK="$USD_PER_MTOK"

  tmp="$REPORT.tmp.$$"
  if ! python3 - "$tmp" <<'PY'
import json
import os
import sys
from datetime import datetime, timezone


def env(name, default=None):
    v = os.environ.get(name)
    return default if v is None or v == "" else v


def as_int(name, default=None):
    v = env(name)
    try:
        return int(v) if v is not None else default
    except (TypeError, ValueError):
        return default


def as_float(name):
    v = env(name)
    try:
        return float(v) if v is not None else None
    except (TypeError, ValueError):
        return None


def read_json(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None


def num(v):
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    return v


phase = env("SMOKE_RPT_PHASE", "failed")
wrapper_exit = as_int("SMOKE_RPT_EXIT", 1)
exit_code = wrapper_exit
reason = env("SMOKE_RPT_REASON", "")
use_paid = env("SMOKE_RPT_USE_PAID", "0") == "1"
out_path = sys.argv[1]
state_path = env("SMOKE_STATE_PATH", "")
log_path = env("SMOKE_LOG_PATH", "")
ws_path = env("SMOKE_WS_PATH", "")
paid_path = env("SMOKE_PAID_REPORT_PATH", "")
commit_fallback = env("SMOKE_RPT_COMMIT_FALLBACK", "unknown")
model_fallback = env("SMOKE_RPT_MODEL_FALLBACK", "")

paid = read_json(paid_path) if use_paid and paid_path else None
if not isinstance(paid, dict):
    paid = None

if paid:
    paid_phase = paid.get("phase")
    if paid_phase in ("passed", "failed", "skipped"):
        phase = paid_phase
    reason = paid.get("error") or reason
    # paid-report 的 phase 是权威，但 wrapper 的退出码不能被它“洗白”：
    # wrapper 非 0 时不允许 report 说 passed。
    if phase == "passed" and wrapper_exit != 0:
        phase = "failed"
        exit_code = wrapper_exit
    elif phase == "failed" and wrapper_exit == 0:
        exit_code = 1
    elif phase == "skipped":
        exit_code = 0 if wrapper_exit == 0 else wrapper_exit
        if wrapper_exit != 0:
            phase = "failed"
    else:
        exit_code = wrapper_exit
    commit = paid.get("commit") or commit_fallback
    model = paid.get("model") or model_fallback
    paid_tokens = paid.get("tokens") if isinstance(paid.get("tokens"), dict) else {}
    tokens_total = num(paid_tokens.get("total"))
    if tokens_total is None:
        tokens_total = 0
    per_round = paid_tokens.get("perRound")
    if not isinstance(per_round, list):
        per_round = []
    tokens_source = paid_tokens.get("source") or "paid-report"
    budget = paid.get("budget") if isinstance(paid.get("budget"), dict) else {}
    rounds_info = paid.get("rounds") if isinstance(paid.get("rounds"), dict) else {}
    timing = paid.get("timing") if isinstance(paid.get("timing"), dict) else {}
    paid_state = paid.get("state") if isinstance(paid.get("state"), dict) else {}
    state_info = {
        "path": paid_state.get("path") or state_path or None,
        "used": bool(paid_state.get("present") or paid_state.get("phase")),
        "phase": paid_state.get("phase"),
        "rounds": paid_state.get("rounds"),
        "roundsPlanned": paid_state.get("roundsPlanned"),
        "tokensTotal": paid_state.get("tokensTotal"),
        "lastError": paid_state.get("lastError"),
        "log": paid_state.get("log") or log_path or None,
        "workspace": paid_state.get("workspace") or ws_path or None,
    }
    if paid.get("paths") and isinstance(paid["paths"], dict):
        log_path = paid["paths"].get("log") or log_path
else:
    commit = commit_fallback
    model = model_fallback or None
    tokens_total = 0
    per_round = []
    tokens_source = "not-run"
    budget = {}
    rounds_info = {}
    timing = {}
    state_info = {
        "path": state_path or None,
        "used": False,
        "phase": None,
        "note": "未调用 tools/soak_paid.sh（SKIP 或配置错误），不读取任何旧 state",
    }

# 退出码与 phase 对齐，避免 report 说 passed 但脚本非 0（或反过来）。
if phase == "passed" and exit_code != 0:
    phase = "failed"
elif phase == "failed" and exit_code == 0:
    exit_code = 1
elif phase == "skipped":
    exit_code = 0

# 调用了 soak_paid 却没有 paid-report：不能凭退出码 0 就宣布通过。
if use_paid and paid is None:
    phase = "failed"
    exit_code = wrapper_exit if wrapper_exit != 0 else 2
    reason = reason or "soak_paid.sh 未写出 paid-report.json：无法证明真实调用结果"
    state_info = {
        "path": state_path or None,
        "used": False,
        "phase": None,
        "note": "调用了 soak_paid.sh，但没有可读取的 paid-report/state；不把缺失当成通过。",
    }

if not budget:
    budget = {
        "rounds": 1,
        "perRound": {
            "maxTokens": as_int("SMOKE_MAX_TOKENS"),
            "maxCalls": as_int("SMOKE_MAX_CALLS"),
            "maxUsd": as_float("SMOKE_MAX_USD"),
            "runTimeoutSec": as_int("SMOKE_RUN_TIMEOUT"),
            "usdPerMtok": as_float("SMOKE_USD_PER_MTOK"),
        },
        "note": "本轮由 tools/soak_paid_smoke.sh 固定为 1 轮。",
    }

price = as_float("SMOKE_USD_PER_MTOK")
budget_price = None
per_round_budget = budget.get("perRound") if isinstance(budget.get("perRound"), dict) else {}
if price is None:
    price = num(per_round_budget.get("usdPerMtok"))
budget_price = price

tokens_int = int(tokens_total or 0)
cost_usd = None
if budget_price is not None and budget_price > 0:
    cost_usd = round(tokens_int * budget_price / 1_000_000.0, 6)

if phase == "skipped":
    cost_note = "没有产生模型调用，本次运行成本视为 0；不伪造 provider 账单。"
elif cost_usd is None:
    cost_note = "未设置每百万 token 单价（UI_SMOKE_USD_PER_MTOK / UI_PAID_USD_PER_MTOK），无法估算金额；账单以 provider 为准。"
else:
    cost_note = "金额是按报告里的 token 数与每百万 token 单价换算的估算值，不是 provider 账单或收据。"

skip_json = env("SMOKE_RPT_SKIP_JSON", "null")
skip_reasons = None
if skip_json and skip_json != "null":
    try:
        parsed = json.loads(skip_json)
        skip_reasons = parsed if isinstance(parsed, list) else [str(parsed)]
    except Exception:
        skip_reasons = [skip_json]

report = {
    "schemaVersion": 1,
    "kind": "paid-smoke",
    "phase": phase,
    "exitCode": exit_code,
    "generatedAt": datetime.now(timezone.utc).isoformat(),
    "startedAt": as_int("SMOKE_RPT_STARTED"),
    "finishedAt": as_int("SMOKE_RPT_FINISHED"),
    "durationMs": (
        timing.get("wallMs")
        if isinstance(timing.get("wallMs"), (int, float))
        else (as_int("SMOKE_RPT_FINISHED", 0) - as_int("SMOKE_RPT_STARTED", 0)) * 1000
    ),
    "timing": timing,
    "commit": commit,
    "model": model,
    "rounds": {
        "planned": 1,
        "completed": rounds_info.get("completed"),
        "ok": rounds_info.get("ok"),
        "source": "paid-report" if paid else "not-run",
    },
    "tokens": {
        "total": tokens_int,
        "perRound": per_round,
        "source": tokens_source,
        "note": "约 tokens，来自 ui run 输出/state；可能为估算值，账单以 provider 为准。",
    },
    "cost": {
        "usd": cost_usd,
        "estimated": cost_usd is not None,
        "usdPerMtok": budget_price,
        "note": cost_note,
    },
    "budget": budget,
    "state": state_info,
    "log": log_path or None,
    "logExists": bool(log_path and os.path.exists(log_path)),
    "paidReport": paid_path if paid else None,
    "skipReasons": skip_reasons,
    "error": reason or None,
    "notes": [
        "paid-smoke 是固定 1 轮的可用性 smoke，不是长跑，也不替代 docs/48 或 docs/50。",
        "报告不包含 API key；key 只通过环境变量传给适配器。",
        "真实调用次数与费用以 provider 账单为准；本文件不伪造调用证据。",
    ],
}

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
  return 0
}

# 最后一道保险：report 里不能出现 key。出现就换成最小失败报告，exit 3。
scan_report_for_key() {
  [ -n "$KEY" ] || return 0
  [ "${#KEY}" -ge 8 ] || return 0
  [ -f "$REPORT" ] || return 0
  SMOKE_SCAN_KEY="$KEY" SMOKE_SCAN_PATH="$REPORT" python3 - 2>/dev/null <<'PY'
import os
import sys

path = os.environ["SMOKE_SCAN_PATH"]
key = os.environ.get("SMOKE_SCAN_KEY", "").encode()
with open(path, "rb") as f:
    data = f.read()
if key and key in data:
    report = {
        "schemaVersion": 1,
        "kind": "paid-smoke",
        "phase": "failed",
        "exitCode": 3,
        "error": "检测到 key 出现在 smoke-report.json，已替换成最小失败报告；请检查适配器是否把环境变量打到了输出。",
        "sanitized": True,
    }
    import json
    with open(path, "w", encoding="utf-8") as f:
        json.dump(report, f, ensure_ascii=False, indent=2)
        f.write("\n")
    sys.exit(1)
sys.exit(0)
PY
}

skip_now() {
  local r
  printf 'SKIP paid-smoke：未进入真实计费运行（没有产生任何模型调用）\n'
  for r in "${SKIP_REASONS[@]}"; do
    printf '  - %s\n' "$r"
  done
  printf '开跑 1 轮 smoke 需要同时满足：\n'
  printf '  UI_PAID_API_KEY（或 OPENAI_API_KEY）+ UI_PAID_MODEL\n'
  printf '  UI_SOAK_ALLOW_COST=1\n'
  printf '  UI_SOAK_YES=1（本入口只接受显式非交互确认）\n'
  printf '说明：docs/55-paid-smoke.md\n'
  if ! write_report "skipped" 0 "" 0; then
    printf 'WARN: skipped smoke-report 未写成: %s\n' "$REPORT" >&2
  fi
  exit 0
}

config_fail() {
  local why="$1"
  printf 'FAIL paid-smoke: %s\n' "$why" >&2
  printf '（配置错误，未产生任何模型调用；修好后重跑）\n' >&2
  if ! write_report "failed" 2 "config: $why" 0; then
    printf 'WARN: failed smoke-report 未写成: %s\n' "$REPORT" >&2
  fi
  exit 2
}

if [ "${#SKIP_REASONS[@]}" -gt 0 ]; then
  skip_now
fi

# ── 固定 1 轮 + 保守预算；非法配置 exit 2，且发生在 soak_paid 之前 ─────
if [ -n "${UI_SMOKE_MAX_USD:-}" ]; then
  MAX_USD="$UI_SMOKE_MAX_USD"
elif [ -n "${UI_SOAK_MAX_USD:-}" ]; then
  MAX_USD="$UI_SOAK_MAX_USD"
else
  MAX_USD=0.01
fi

if [ -n "${UI_SMOKE_MAX_TOKENS:-}" ]; then
  MAX_TOKENS="$UI_SMOKE_MAX_TOKENS"
elif [ -n "${UI_SOAK_MAX_TOKENS:-}" ]; then
  MAX_TOKENS="$UI_SOAK_MAX_TOKENS"
else
  MAX_TOKENS=30000
fi

if [ -n "${UI_SMOKE_MAX_CALLS:-}" ]; then
  MAX_CALLS="$UI_SMOKE_MAX_CALLS"
elif [ -n "${UI_SOAK_MAX_CALLS:-}" ]; then
  MAX_CALLS="$UI_SOAK_MAX_CALLS"
else
  MAX_CALLS=20
fi

if [ -n "${UI_SMOKE_RUN_TIMEOUT:-}" ]; then
  RUN_TIMEOUT="$UI_SMOKE_RUN_TIMEOUT"
elif [ -n "${UI_SOAK_RUN_TIMEOUT:-}" ]; then
  RUN_TIMEOUT="$UI_SOAK_RUN_TIMEOUT"
elif [ -n "${UI_PAID_RUN_TIMEOUT:-}" ]; then
  RUN_TIMEOUT="$UI_PAID_RUN_TIMEOUT"
else
  RUN_TIMEOUT=300
fi

USD_PER_MTOK="${UI_SMOKE_USD_PER_MTOK:-${UI_PAID_USD_PER_MTOK:-${UI_SOAK_USD_PER_MTOK:-}}}"

is_positive_decimal "$MAX_USD" || config_fail "max_usd 必须是大于 0 的数字: $MAX_USD"
is_pos_int "$MAX_TOKENS" || config_fail "max_tokens 必须是正整数: $MAX_TOKENS"
is_pos_int "$MAX_CALLS" || config_fail "max_calls 必须是正整数: $MAX_CALLS"
is_uint "$RUN_TIMEOUT" || config_fail "run_timeout 必须是非负整数: $RUN_TIMEOUT"
if [ -n "$USD_PER_MTOK" ]; then
  is_positive_decimal "$USD_PER_MTOK" || config_fail "usd_per_mtok 必须是大于 0 的数字: $USD_PER_MTOK"
fi

# 去掉前导 0，避免 bash 算术把 08 当八进制。
MAX_TOKENS=$((10#$MAX_TOKENS))
MAX_CALLS=$((10#$MAX_CALLS))
RUN_TIMEOUT=$((10#$RUN_TIMEOUT))

USD_ENFORCED=0
if [ -n "$USD_PER_MTOK" ]; then
  USD_ENFORCED=1
fi

# 默认适配器需要 node；本入口在调用 soak_paid 前先检查，避免白走确认流程。
if [ -z "${UI_PAID_MODEL_CMD:-}" ]; then
  if ! command -v node >/dev/null 2>&1; then
    config_fail "找不到 node：默认适配器是 node tools/chat_compat.mjs；可装 node，或用 UI_PAID_MODEL_CMD 覆盖"
  fi
  if [ ! -f "$UI_ROOT/tools/chat_compat.mjs" ]; then
    config_fail "缺少 tools/chat_compat.mjs：默认适配器不存在"
  fi
fi

# ── 运行前把代价打印清楚，再交给 soak_paid.sh ───────────────────────────
printf 'PAID-SMOKE 1 轮真实 provider smoke\n'
printf '  这会真实计费：1 轮带工具的任务，不是 mock，也不是长跑。\n'
printf '  模型: %s\n' "$MODEL"
printf '  轮数: 1（固定；忽略 UI_SOAK_ROUNDS）\n'
printf '  预算: max_usd=%s, max_tokens=%s, max_calls=%s, run_timeout=%ss\n' \
  "$MAX_USD" "$MAX_TOKENS" "$MAX_CALLS" "$RUN_TIMEOUT"
if [ "$USD_ENFORCED" = 1 ]; then
  printf '  USD 硬中止: 已启用（--usd-per-mtok=%s）；达到 $%s 即中止本轮。\n' "$USD_PER_MTOK" "$MAX_USD"
else
  printf '  USD 硬中止: 未启用（未设置 UI_SMOKE_USD_PER_MTOK / UI_PAID_USD_PER_MTOK）。\n'
  printf '  --max-usd 只是名义值；真正硬中止的是 max_tokens / max_calls / 墙钟。\n'
fi
printf '  计费风险: 触顶只中止后续调用，已发生的调用仍会计费；适配器可能内部重试；token 统计不是账单。\n'
printf '  产物: report=%s state=%s log=%s\n' "$REPORT" "$STATE" "$LOG"
printf '  说明: 1 轮 smoke 不替代长跑；见 docs/55-paid-smoke.md。\n'

if [ -n "${UI_SOAK_ROUNDS:-}" ] && [ "${UI_SOAK_ROUNDS}" != "1" ]; then
  printf '  注意: 已忽略环境里的 UI_SOAK_ROUNDS=%s，本入口固定 1 轮。\n' "$UI_SOAK_ROUNDS"
fi

mkdir -p "$(dirname "$REPORT")" "$(dirname "$PAID_REPORT")" \
  "$(dirname "$STATE")" "$(dirname "$LOG")" "$(dirname "$ROUNDS_LOG")" 2>/dev/null || true
# 不读取上一次的 paid/smoke report；state 由 soak_real 覆盖并按 startedAt 校验。
rm -f "$PAID_REPORT" "$REPORT" 2>/dev/null || true

export UI_SOAK_ALLOW_COST=1
export UI_SOAK_ROUNDS=1
export UI_SOAK_INTERVAL=0
export UI_SOAK_RUN_TIMEOUT="$RUN_TIMEOUT"
export UI_PAID_RUN_TIMEOUT="$RUN_TIMEOUT"
export UI_SOAK_REQUIRE_TOOL=1
export UI_SOAK_MAX_TOKENS="$MAX_TOKENS"
export UI_SOAK_MAX_CALLS="$MAX_CALLS"
export UI_SOAK_MAX_USD="$MAX_USD"
export UI_SOAK_WS="$WS"
export UI_SOAK_LOG="$LOG"
export UI_SOAK_STATE="$STATE"
export UI_SOAK_ROUNDS_LOG="$ROUNDS_LOG"
export UI_SOAK_MODEL_LABEL="$MODEL"
export UI_SOAK_KEEP_WS=0
export UI_PAID_REPORT="$PAID_REPORT"

# 不把长跑/其他任务的残留配置带进 smoke。
unset UI_SOAK_MAX_STEPS 2>/dev/null || true
unset UI_SOAK_TASK 2>/dev/null || true

if [ -n "$USD_PER_MTOK" ]; then
  export UI_SOAK_USD_PER_MTOK="$USD_PER_MTOK"
  export UI_PAID_USD_PER_MTOK="$USD_PER_MTOK"
else
  unset UI_SOAK_USD_PER_MTOK 2>/dev/null || true
  unset UI_PAID_USD_PER_MTOK 2>/dev/null || true
fi

printf '\n启动 tools/soak_paid.sh（固定 1 轮；失败即停，不自动重试）...\n'
set +e
bash "$UI_ROOT/tools/soak_paid.sh"
rc=$?
set -e

if [ "$rc" -eq 0 ]; then
  PHASE="passed"
  REASON=""
elif [ "$rc" -eq 2 ]; then
  PHASE="failed"
  REASON="soak_paid 配置错误（exit 2）"
elif [ "$rc" -eq 130 ]; then
  PHASE="failed"
  REASON="interrupted by signal"
else
  PHASE="failed"
  REASON="soak_paid exit=$rc"
fi

# soak_paid 的 report 才是权威；若它没写成，按 wrapper 自己的退出码记 failed。
if ! write_report "$PHASE" "$rc" "$REASON" 1; then
  printf 'WARN: smoke-report 未写成: %s\n' "$REPORT" >&2
  [ "$rc" -eq 0 ] && rc=2
fi

if ! scan_report_for_key; then
  rc=3
fi

summary=$(SMOKE_REPORT_PATH="$REPORT" python3 - 2>/dev/null <<'PY'
import json
import os

try:
    with open(os.environ["SMOKE_REPORT_PATH"], encoding="utf-8") as f:
        r = json.load(f)
    cost = r.get("cost") or {}
    print(
        "phase=%s commit=%s model=%s tokens=%s cost_usd=%s"
        % (
            r.get("phase"),
            r.get("commit") or "unknown",
            r.get("model") or "unknown",
            (r.get("tokens") or {}).get("total"),
            "unknown" if cost.get("usd") is None else cost.get("usd"),
        )
    )
except Exception:
    print("phase=unknown commit=unknown model=unknown tokens=0 cost_usd=unknown")
PY
) || summary="phase=unknown commit=unknown model=unknown tokens=0 cost_usd=unknown"

if [ "$rc" -eq 0 ]; then
  printf 'PASS paid-smoke %s report=%s state=%s log=%s\n' "$summary" "$REPORT" "$STATE" "$LOG"
else
  printf 'FAIL paid-smoke rc=%s %s report=%s state=%s log=%s\n' "$rc" "$summary" "$REPORT" "$STATE" "$LOG" >&2
fi
exit "$rc"
