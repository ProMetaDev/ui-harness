#!/usr/bin/env bash
# 读取 soak state，检查 phase/rounds/心跳，并显示指标来源与最近一轮明细。
# 用法: tools/soak_status.sh [state-file]
# 退出码: 0=passed/健康 running；1=interrupted/心跳过期/进程已死；2=failed/state 不可读。
set -euo pipefail

# Windows+core.autocrlf=true 的 worktree 会把 env.sh 写成 CRLF（env.sh 不在本次改动范围），
# 用 /tmp 的 LF 副本加载；从副本加载时 env.sh 的 UI_ROOT 会算错，下面按真实位置纠正。
_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if grep -q $'\r' "$_here/env.sh" 2>/dev/null; then
  _env_tmp="${TMPDIR:-/tmp}/ui-env-status-$(id -u).sh"
  tr -d '\r' < "$_here/env.sh" >"$_env_tmp"
  # shellcheck disable=SC1090
  source "$_env_tmp"
else
  # shellcheck disable=SC1091
  source "$_here/env.sh"
fi
UI_ROOT="$(cd "$_here/.." && pwd)"
export UI_ROOT
export UI_GIT_DIR="$UI_ROOT"

STATE="${1:-${UI_SOAK_STATE:-/tmp/ui-soak-long-state.json}}"
[ -f "$STATE" ] || { echo "FAIL: 没有 state 文件: $STATE"; exit 2; }

python3 - "$STATE" <<'PY'
import json, os, sys, time

state = sys.argv[1]
try:
    with open(state, encoding="utf-8") as f:
        d = json.load(f)
except Exception as e:  # noqa: BLE001 - 任何损坏都按不可读处理
    print(f"FAIL: 读不了 state ({state}): {e}", file=sys.stderr)
    sys.exit(2)
if not isinstance(d, dict):
    print("FAIL: state 不是 JSON 对象", file=sys.stderr)
    sys.exit(2)

now = int(time.time())
try:
    updated = int(d.get("updatedAt") or 0)
except (TypeError, ValueError):
    updated = 0
age = max(0, now - updated)
try:
    interval = max(1, int(d.get("interval") or 1))
except (TypeError, ValueError):
    interval = 1
phase = d.get("phase")
rounds = d.get("rounds", 0)
print(f"phase={phase} rounds={rounds} age={age}s interval={interval}s state={state}")

if d.get("workspace") or d.get("log"):
    print(f"workspace={d.get('workspace', '?')} log={d.get('log', '?')}")

m = d.get("metrics")
if isinstance(m, dict):
    print(
        "metrics: "
        f"mode={d.get('metricMode', '?')} rss={m.get('rss', '?')} "
        f"rssSource={m.get('rssSource', '?')} fdSource={m.get('fdSource', '?')}"
    )
else:
    print("WARN: state 没有 metrics 字段：指标来源未知", file=sys.stderr)
    if d.get("schemaVersion") is None:
        print(
            "WARN: 旧版 state（无 schemaVersion）：旧脚本可能采的是 shell RSS，不能当 ui 指标",
            file=sys.stderr,
        )

def stage_text(s):
    if not isinstance(s, dict):
        return "null"
    return (
        f"rc={s.get('rc')} ms={s.get('ms')} rssKB={s.get('rssKB')} "
        f"rssSource={s.get('rssSource')} fdMax={s.get('fdMax')} "
        f"fdSource={s.get('fdSource')} timedOut={s.get('timedOut')}"
    )

lr = d.get("lastRound")
if isinstance(lr, dict):
    print(
        f"lastRound={lr.get('round')} ok={lr.get('ok')} "
        f"journalChainOk={lr.get('journalChainOk')}"
    )
    print(f"  run:    {stage_text(lr.get('run'))}")
    print(f"  verify: {stage_text(lr.get('verify'))}")
else:
    print("lastRound=null")

if d.get("lastError"):
    print(f"lastError={d.get('lastError')}")
if d.get("failedRounds"):
    print(f"failedRounds={d.get('failedRounds')}")

# 新 state 带 runnerPid：phase=running 时必须真的还活着，否则是陈旧 state。
if phase == "running" and os.path.isdir("/proc"):
    pid = d.get("runnerPid")
    if isinstance(pid, int) and pid > 0:
        if not os.path.isdir(f"/proc/{pid}"):
            print(
                f"FAIL: state 声称 running，但 runnerPid={pid} 已不存在（soak 进程已死）",
                file=sys.stderr,
            )
            sys.exit(1)
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode("utf-8", "replace")
            if "soak_long" not in cmd:
                print(f"WARN: runnerPid={pid} 存活但命令行不像 soak_long: {cmd[:160]}", file=sys.stderr)
        except OSError:
            pass

if phase == "failed":
    sys.exit(2)
if phase == "passed":
    sys.exit(0)
if phase == "interrupted":
    print("interrupted：可用 UI_SOAK_RESUME=1 续跑", file=sys.stderr)
    sys.exit(1)
if phase != "running":
    print(f"FAIL: 未知 phase: {phase!r}", file=sys.stderr)
    sys.exit(2)

# running 但超过 3 个间隔没有心跳：进程可能卡死。
limit = max(interval * 3, 300)
if age > limit:
    print(f"FAIL: 心跳过期 ({age}s > {limit}s)", file=sys.stderr)
    sys.exit(1)

if rounds and isinstance(lr, dict) and lr.get("round") != rounds:
    print(
        f"WARN: rounds={rounds} 与 lastRound.round={lr.get('round')} 不一致（state 可能刚更新到一半）",
        file=sys.stderr,
    )
PY
