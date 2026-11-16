#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 对同一个任务重复跑 N 次，统计 tokens / steps / calls / wall time。
# 用途：判断「某个改动到底有没有效果」——同一剧本跑基线，改完再用同一剧本跑一次。
#
#   bash tools/eval.sh --task demo/task.txt \
#     --workspace fixtures/eval-workspace \
#     --mock fixtures/scenarios/fix-bug.json \
#     --runs 3 --label baseline [--tool-chars 2000]
#
# 关键点：
#   * --workspace 只是模板：每次跑都复制一份全新工作区到
#     out/eval/<label>-<时间戳>/run-N/workspace，原目录不会被写。
#     不传 --workspace 时用 fixtures/eval-workspace。
#   * 只跑 release 二进制（默认 "$UI_BIN"），不调用真实模型：
#     --mock 是必填项，剧本决定每一步，结果可复现。
#   * 任何一次 reason != done（或 ui 退出码非 0）→ 退出码 1，可以直接当门禁。
#
# 自测（3 次 mock，全部 done，JSON 可解析，退出码 0）：
#   bash tools/eval.sh --task demo/task.txt --runs 3 \
#     --mock fixtures/scenarios/fix-bug.json --label selftest
#   echo "eval exit=$?（期望 0）"
#   python3 - <<'PY'
#   import glob, json
#   p = sorted(glob.glob("out/eval/selftest-[0-9]*.json"))[-1]
#   d = json.load(open(p, encoding="utf-8"))
#   assert len(d["runs"]) == 3, d
#   assert d["summary"]["tokens"]["median"] > 0, d["summary"]
#   assert d["summary"]["tokens"]["min"] > 0
#   assert d["summary"]["tokens"]["max"] > 0
#   assert all(r["reason"] == "done" for r in d["runs"]), d["runs"]
#   print("JSON OK:", p)
#   PY
#
# 负向自测（门禁必须报红）：
#   bash tools/eval.sh --task demo/task.txt --runs 1 \
#     --mock fixtures/scenarios/eval-not-done.json --label should-fail
#   echo "eval exit=$?（期望 1）"
#
# 退出码：0 = 全部 done；1 = 有 run 不是 done / 解析失败；2 = 用法或环境错误。
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

fail() {
  echo "eval: $*" >&2
  exit 2
}

usage() {
  cat <<'USAGE'
用法：bash tools/eval.sh --task <文件> --mock <剧本> [选项]

  --task <文件>       任务文件（传给 ui run --task-file），必填
  --workspace <目录>  工作区模板；不传则用 fixtures/eval-workspace
  --runs N            重复次数，默认 3
  --mock <剧本>       脚本化模型剧本，必填（评测不调用真实模型）
  --label <名字>      标签，用于 out/eval/<label>-<时间戳>.json，默认 eval
  --tool-chars N      透传给 ui run；不传则用 harness 默认值（不截断）
  --timeout 秒        单次运行的硬超时，默认 900
  -h, --help          显示本帮助

每次运行都会从 --workspace 复制一份干净副本。只要有一次结束原因不是
done，脚本退出码就是 1；全部 done 才是 0。
USAGE
}

# ---------- 参数 ----------
TASK=""
WORKSPACE=""
MOCK=""
LABEL="eval"
RUNS=3
TOOL_CHARS=""
TIMEOUT=900

while [ $# -gt 0 ]; do
  case "$1" in
    --task)
      [ $# -ge 2 ] || fail "--task 缺少文件路径"
      TASK="$2"; shift 2 ;;
    --workspace)
      [ $# -ge 2 ] || fail "--workspace 缺少目录"
      WORKSPACE="$2"; shift 2 ;;
    --mock)
      [ $# -ge 2 ] || fail "--mock 缺少剧本路径"
      MOCK="$2"; shift 2 ;;
    --label)
      [ $# -ge 2 ] || fail "--label 缺少名字"
      LABEL="$2"; shift 2 ;;
    --runs)
      [ $# -ge 2 ] || fail "--runs 缺少数值"
      RUNS="$2"; shift 2 ;;
    --tool-chars)
      [ $# -ge 2 ] || fail "--tool-chars 缺少数值"
      TOOL_CHARS="$2"; shift 2 ;;
    --timeout)
      [ $# -ge 2 ] || fail "--timeout 缺少数值"
      TIMEOUT="$2"; shift 2 ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      fail "未知参数 $1（用 --help 看用法）" ;;
  esac
done

[ -n "$TASK" ] || fail "缺少 --task <任务文件>"
[ -f "$TASK" ] || fail "任务文件不存在: $TASK"
[ -n "$MOCK" ] || fail "缺少 --mock <剧本>（评测必须用脚本化模型，避免真实 API 费用）"
[ -f "$MOCK" ] || fail "剧本不存在: $MOCK"
[[ "$RUNS" =~ ^[1-9][0-9]*$ ]] || fail "--runs 必须是正整数: $RUNS"
[[ "$TIMEOUT" =~ ^[1-9][0-9]*$ ]] || fail "--timeout 必须是正整数: $TIMEOUT"
if [ -n "$TOOL_CHARS" ]; then
  [[ "$TOOL_CHARS" =~ ^[0-9]+$ ]] || fail "--tool-chars 必须是非负整数: $TOOL_CHARS"
fi

TASK_ABS="$(realpath "$TASK")" || fail "无法解析任务路径: $TASK"
MOCK_ABS="$(realpath "$MOCK")" || fail "无法解析剧本路径: $MOCK"
WS_ARG="${WORKSPACE:-fixtures/eval-workspace}"
[ -d "$WS_ARG" ] || fail "工作区不存在: $WS_ARG"
WS_SRC="$(realpath "$WS_ARG")" || fail "无法解析工作区路径: $WS_ARG"

# 标签会进文件名：只挡掉目录分隔符和空白，中文保留
LABEL="${LABEL:-eval}"
LABEL="${LABEL//\//_}"
LABEL="${LABEL//[[:space:]]/_}"

# ---------- 环境 ----------
# 默认按项目约定用 "$UI_BUILD_DIR" 下的 release 二进制；HOME 被 Windows 环境带歪时
# 再按真实用户目录 / 另一份 target 顺序找，避免 eval 本身因为环境问题跑不起来。
USER_HOME="$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6)"
DEFAULT_HOME="${HOME:-${USER_HOME:-/home/$(id -un)}}"
BIN="${UI_BIN:-$DEFAULT_HOME/ui/target/release/ui}"
if [ ! -x "$BIN" ]; then
  for c in "${USER_HOME:-$DEFAULT_HOME}/ui/target/release/ui" "$DEFAULT_HOME/ui-target/release/ui" "$ROOT/target/release/ui"; do
    if [ -x "$c" ]; then BIN="$c"; break; fi
  done
fi
[ -x "$BIN" ] || fail "找不到 release 二进制: ${UI_BIN:-$DEFAULT_HOME/ui/target/release/ui}（先跑 bash tools/dev.sh build --release）"
command -v python3 >/dev/null 2>&1 || fail "需要 python3 来生成统计和 JSON"

# ---------- 输出目录 ----------
STAMP="$(date +%Y%m%d-%H%M%S)"
BASE="$ROOT/out/eval/$LABEL-$STAMP"
# 同一秒跑两次也不用覆盖：加 pid 后缀
if [ -e "$BASE" ] || [ -e "$BASE.json" ]; then
  BASE="$BASE-$$"
fi
mkdir -p "$BASE" || fail "无法创建输出目录: $BASE"
JSON="$BASE.json"
TSV="$BASE/runs.tsv"
STARTED_AT="$(date +%Y-%m-%dT%H:%M:%S%:z)"

echo "== 评测 ${LABEL} =="
printf 'task       %s\n' "$TASK"
printf 'mock       %s\n' "$MOCK"
printf 'workspace  %s（每次复制一份干净副本）\n' "$WS_ARG"
printf 'runs       %s\n' "$RUNS"
if [ -n "$TOOL_CHARS" ]; then
  printf 'tool-chars %s\n' "$TOOL_CHARS"
fi
printf 'bin        %s\n' "$BIN"
printf 'out        %s\n' "$BASE"
echo

# ---------- 复制工作区 ----------
# .ui / target / out / .git / secrets 属于运行产物或敏感目录，不带进副本。
copy_workspace() {
  local src="$1" dst="$2"
  mkdir -p "$dst" || return 1
  if command -v rsync >/dev/null 2>&1; then
    rsync -a \
      --exclude '.ui' --exclude 'target' --exclude 'out' \
      --exclude '.git' --exclude 'secrets' \
      "$src"/ "$dst"/ || return 1
  else
    cp -a "$src"/. "$dst"/ || return 1
  fi
  rm -rf "$dst/.ui" "$dst/target" "$dst/out" "$dst/.git" "$dst/secrets"
}

# ---------- 单次运行 ----------
# 从 stdout 抓最后一行汇总：「结束原因 done · 4 步 · 3 次工具 · 约 1719 tokens」。
# 先抓完整短语再取数字，避免正文里恰好出现「N 步」时抓错。
parse_num() {
  local pattern="$1" file="$2"
  grep -oE "$pattern" "$file" 2>/dev/null | tail -n 1 | grep -oE '[0-9]+' || true
}

EXTRA=()
if [ -n "$TOOL_CHARS" ]; then
  EXTRA=(--tool-chars "$TOOL_CHARS")
fi

printf 'run\ttokens\tsteps\tcalls\twall_ms\treason\texit\n' > "$TSV"

EVAL_FAIL=0
PARSE_FAIL=0
NOT_DONE=0

for ((i = 1; i <= RUNS; i++)); do
  RUN_DIR="$BASE/run-$i"
  RUN_WS="$RUN_DIR/workspace"
  mkdir -p "$RUN_DIR" || fail "无法创建运行目录: $RUN_DIR"
  copy_workspace "$WS_SRC" "$RUN_WS" || fail "第 $i 次：复制工作区失败（$WS_SRC → $RUN_WS）"

  printf 'run %d/%d ... ' "$i" "$RUNS"
  START_MS="$(date +%s%3N)"
  # --yes：mock 剧本会写文件，评测必须无人值守地批准；
  # timeout：剧本写错时不要挂住整批评测。
  timeout "$TIMEOUT" "$BIN" run \
    --task-file "$TASK_ABS" \
    --workspace "$RUN_WS" \
    --mock "$MOCK_ABS" \
    --yes "${EXTRA[@]}" \
    > "$RUN_DIR/stdout.log" 2> "$RUN_DIR/stderr.log"
  CODE=$?
  END_MS="$(date +%s%3N)"
  WALL_MS=$((END_MS - START_MS))
  [ "$WALL_MS" -ge 0 ] || WALL_MS=0

  OUT="$RUN_DIR/stdout.log"
  TOKENS="$(parse_num '约 [0-9]+ tokens' "$OUT")"
  STEPS="$(parse_num '[0-9]+ 步' "$OUT")"
  CALLS="$(parse_num '[0-9]+ 次工具' "$OUT")"
  REASON="$(grep -oE '结束原因 [a-z_]+' "$OUT" 2>/dev/null | tail -n 1 | awk '{print $2}' || true)"
  REASON="${REASON:-no_final_line}"

  printf '%s · %s tokens · %sms\n' "$REASON" "${TOKENS:--}" "$WALL_MS"

  if [ -z "$TOKENS" ] || [ -z "$STEPS" ] || [ -z "$CALLS" ]; then
    echo "  警告：stdout 里没解析全（tokens='$TOKENS' steps='$STEPS' calls='$CALLS'），见 $OUT" >&2
    PARSE_FAIL=1
  fi
  if [ "$CODE" -ne 0 ] || [ "$REASON" != "done" ]; then
    EVAL_FAIL=1
    NOT_DONE=$((NOT_DONE + 1))
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$i" "$TOKENS" "$STEPS" "$CALLS" "$WALL_MS" "$REASON" "$CODE" >> "$TSV"
done

# ---------- 统计 + JSON ----------
# 统计和 JSON 都交给 python3：中位数、空值、字符串转义不必在 bash 里重写一遍。
python3 - "$TSV" "$JSON" "$LABEL" "$TASK_ABS" "$MOCK_ABS" "$WS_SRC" \
  "$BIN" "$STARTED_AT" "$TOOL_CHARS" "$RUNS" <<'PY'
import json
import statistics
import sys

(tsv, out_json, label, task, mock, workspace, binary,
 started_at, tool_chars, requested_runs) = sys.argv[1:11]

runs = []
with open(tsv, encoding="utf-8") as f:
    header = f.readline().rstrip("\n").split("\t")
    for line in f:
        line = line.rstrip("\n")
        if not line:
            continue
        row = dict(zip(header, line.split("\t")))

        def num(key):
            raw = row.get(key, "")
            return int(raw) if raw.strip() else None

        runs.append({
            "run": int(row["run"]),
            "tokens": num("tokens"),
            "steps": num("steps"),
            "calls": num("calls"),
            "wall_ms": num("wall_ms"),
            "reason": row.get("reason", ""),
            "exit": num("exit"),
        })

def stats(key):
    vals = [r[key] for r in runs if r[key] is not None]
    if not vals:
        return {"median": None, "min": None, "max": None}
    med = statistics.median(vals)
    if isinstance(med, float) and med.is_integer():
        med = int(med)
    return {"median": med, "min": min(vals), "max": max(vals)}

done = [r for r in runs if r["reason"] == "done" and r["exit"] == 0]
summary = {
    "runs": len(runs),
    "done": len(done),
    "all_done": len(runs) > 0 and len(done) == len(runs),
    "tokens": stats("tokens"),
    "steps": stats("steps"),
    "calls": stats("calls"),
    "wall_ms": stats("wall_ms"),
}

data = {
    "label": label,
    "started_at": started_at,
    "task": task,
    "mock": mock,
    "workspace_source": workspace,
    "binary": binary,
    "requested_runs": int(requested_runs),
    "tool_chars": int(tool_chars) if tool_chars else None,
    "runs": runs,
    "summary": summary,
}
with open(out_json, "w", encoding="utf-8") as f:
    json.dump(data, f, ensure_ascii=False, indent=2)
    f.write("\n")

def fmt_num(v):
    if v is None:
        return "-"
    if isinstance(v, float) and not v.is_integer():
        return f"{v:.1f}"
    return str(int(v))

def fmt_sec(ms):
    return "-" if ms is None else f"{ms / 1000:.3f}"

# 一张表：每次一行，再加中位数 / 最小 / 最大
print(f"{'run':<7}{'tokens':>8}{'steps':>7}{'calls':>7}{'wall_s':>9}{'reason':>16}{'exit':>6}")
for r in runs:
    print(f"{r['run']:<7}{fmt_num(r['tokens']):>8}{fmt_num(r['steps']):>7}"
          f"{fmt_num(r['calls']):>7}{fmt_sec(r['wall_ms']):>9}"
          f"{r['reason']:>16}{fmt_num(r['exit']):>6}")
for name in ("median", "min", "max"):
    print(f"{name:<7}{fmt_num(summary['tokens'][name]):>8}"
          f"{fmt_num(summary['steps'][name]):>7}"
          f"{fmt_num(summary['calls'][name]):>7}"
          f"{fmt_sec(summary['wall_ms'][name]):>9}{'-':>16}{'-':>6}")
PY
PY_RC=$?
[ "$PY_RC" -eq 0 ] || fail "生成统计/JSON 失败（python3 退出码 $PY_RC）"

echo
echo "runs 明细：$BASE/run-N/{stdout.log,stderr.log,workspace}"
echo "json：$JSON"

if [ "$EVAL_FAIL" -eq 0 ] && [ "$PARSE_FAIL" -eq 0 ]; then
  echo "结果：$RUNS/$RUNS done → PASS（退出码 0）"
  exit 0
fi

if [ "$PARSE_FAIL" -ne 0 ]; then
  echo "结果：有 run 的汇总字段解析失败 → FAIL（退出码 1）" >&2
else
  echo "结果：$NOT_DONE/$RUNS 次没有正常完成（理由非 done 或退出码非 0）→ FAIL（退出码 1）" >&2
fi
exit 1
