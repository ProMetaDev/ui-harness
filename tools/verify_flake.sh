#!/usr/bin/env bash
# lab_pack 低频 flake 稳定性循环：默认单测 20 次 + 全量 3 次。
# 不修改/放宽任何断言；失败时保留日志、临时 workspace、journal、inputs.jsonl，
# 并自动做并行/文件时序/自动 checkpoint 三类假设的取证。
#
# 用法: bash tools/verify_flake.sh
#   UI_FLAKE_ITERS=20        单测次数
#   UI_FLAKE_FULL_ITERS=3    全量 cargo test --workspace 次数
#   UI_FLAKE_OUT=/tmp/...    日志目录（默认按时间戳新建）
#   UI_FLAKE_DIAG=1          失败时追加串行/并行对照重跑（默认 1）
set -uo pipefail

# Windows+core.autocrlf=true 的 worktree 会把 env.sh 写成 CRLF（env.sh 不在本次改动范围），
# 用 /tmp 的 LF 副本加载；从副本加载时 env.sh 的 UI_ROOT 会算错，下面按真实位置纠正。
_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if grep -q $'\r' "$_here/env.sh" 2>/dev/null; then
  _env_tmp="${TMPDIR:-/tmp}/ui-env-flake-$(id -u).sh"
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
cd "$UI_ROOT"

N="${UI_FLAKE_ITERS:-20}"
FULL="${UI_FLAKE_FULL_ITERS:-3}"
DIAG="${UI_FLAKE_DIAG:-1}"
TEST="lab_pack_keeps_artifacts_flat_and_not_as_inputs"
OUT="${UI_FLAKE_OUT:-/tmp/ui-flake-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"

case "$N" in ''|*[!0-9]*) echo "FAIL: UI_FLAKE_ITERS 必须是整数: $N" >&2; exit 2 ;; esac
case "$FULL" in ''|*[!0-9]*) echo "FAIL: UI_FLAKE_FULL_ITERS 必须是整数: $FULL" >&2; exit 2 ;; esac

FAILS=0
FAIL_LIST=""
now_ms() {
  local t
  t=$(date +%s%3N 2>/dev/null || true)
  case "$t" in
    ''|*[!0-9]*) printf '%s000' "$(date +%s)" ;;
    *) printf '%s' "$t" ;;
  esac
}

{
  echo "time=$(date -Is 2>/dev/null || date)"
  echo "host=$(hostname 2>/dev/null || echo '?')"
  echo "nproc=$(nproc 2>/dev/null || echo '?')"
  echo "loadavg=$(cat /proc/loadavg 2>/dev/null || echo '?')"
  echo "UI_ROOT=$UI_ROOT"
  echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-}"
  echo "TMPDIR=${TMPDIR:-/tmp}"
  echo "test_filter=$TEST"
  rustc --version 2>&1 || true
  cargo --version 2>&1 || true
  echo "--- 开始前遗留的 labpack workspace ---"
  # shellcheck disable=SC2012
  ls -d /tmp/ui-cli-*-labpack 2>/dev/null || echo "none"
} >"$OUT/env.txt" 2>&1
cat "$OUT/env.txt"

# ── 失败现场取证 ────────────────────────────────────────────────────
journal_digest() {
  local j=$1
  command -v python3 >/dev/null 2>&1 || return 0
  python3 - "$j" <<'PY'
import json, sys, os
path = sys.argv[1]
kinds = {}
first_ts = None
checkpoints = []
reads = []
writes = []
for n, line in enumerate(open(path, encoding="utf-8", errors="replace"), 1):
    try:
        e = json.loads(line)
    except Exception:
        continue
    k = e.get("kind", "?")
    kinds[k] = kinds.get(k, 0) + 1
    if first_ts is None:
        first_ts = e.get("ts")
    if k == "fs/read":
        reads.append((n, e.get("path"), e.get("tool")))
    if k == "tool/request":
        for f in e.get("effects") or []:
            if f.get("kind") in ("fs.write", "fs.edit"):
                writes.append((n, e.get("tool"), f.get("target")))
        if e.get("tool") == "fs_write":
            pass
    if k in ("checkpoint/auto", "checkpoint/error"):
        checkpoints.append((n, k, e.get("error") or e.get("label") or e.get("id") or ""))
print("journal events:", sum(kinds.values()), "first_ts_ms:", first_ts)
print("kinds:", dict(sorted(kinds.items())))
print("checkpoint 事件:", checkpoints if checkpoints else "none")
print("fs/read 顺序:", reads)
print("工具写效果:", writes)
PY
}

mtime_digest() {
  local dir=$1
  command -v python3 >/dev/null 2>&1 || return 0
  python3 - "$dir" <<'PY'
import os, sys, time
root = sys.argv[1]
j = os.path.join(root, ".ui", "journal.jsonl")
first = None
try:
    import json
    for line in open(j, encoding="utf-8", errors="replace"):
        try: first = json.loads(line).get("ts"); break
        except Exception: pass
except OSError:
    pass
print("journal 首事件 ts(ms):", first)
for rel in ("sc.json", "fixtures_input.txt", "artifacts/metrics.json"):
    p = os.path.join(root, rel)
    try:
        st = os.stat(p)
        print(f"  {rel}: mtime_ms={int(st.st_mtime*1000)} size={st.st_size}")
    except OSError:
        print(f"  {rel}: missing")
PY
}

analyze_snapshot() {
  local snap=$1
  echo
  echo "### 现场: $snap"
  echo "--- 文件 ---"
  find "$snap" -maxdepth 5 -type f 2>/dev/null | sort | head -80
  echo "--- inputs.jsonl ---"
  local inputs
  inputs=$(find "$snap" -path '*/experiments/*/inputs.jsonl' 2>/dev/null | head -1)
  if [ -n "$inputs" ]; then cat "$inputs"; else echo "（没有 inputs.jsonl：pack 可能没跑完）"; fi
  echo "--- bundle artifacts 结构（检查是否多套一层） ---"
  find "$snap" -path '*experiments*' -name artifacts -type d 2>/dev/null | head -10
  echo "--- workspace 关键文件 mtime 与 journal 首事件 ---"
  local ws
  ws=$(find "$snap" -maxdepth 1 -mindepth 1 -type d -name 'ui-cli-*' 2>/dev/null | head -1)
  [ -n "$ws" ] && mtime_digest "$ws"
  echo "--- journal 摘要 ---"
  local j
  j=$(find "$snap" -name journal.jsonl 2>/dev/null | head -1)
  [ -n "$j" ] && journal_digest "$j"
  echo "--- 三类假设 ---"
  echo "  并行：同一 cli 测试二进制里其他测试也在写 /tmp；看下面的串行对照重跑。"
  echo "  文件时序：lab pack 的 mtime 弱血缘用 run_start(ms) 比较文件 mtime；上面打印了两者。"
  echo "  自动 checkpoint：设置 UI_AUTO_CHECKPOINT=0 的对照需人工另跑；上面已列出 checkpoint/auto 事件。"
}

snapshot_failure() {
  local kind=$1 idx=$2
  local log="$OUT/$kind-$idx.log"
  local snap="$OUT/failure-$kind-$idx"
  mkdir -p "$snap"
  cp -f "$log" "$snap/test.log" 2>/dev/null || true
  local d base
  for d in /tmp/ui-cli-*-labpack; do
    [ -d "$d" ] || continue
    base=$(basename "$d")
    cp -a "$d" "$snap/$base" 2>/dev/null || true
  done
  {
    echo "### $kind iteration $idx 失败"
    echo "--- 日志里的断言/panic ---"
    grep -nE 'panicked|assertion|left:|right:|失败|出错' "$log" 2>/dev/null | head -40 || true
    echo
    echo "--- 日志尾部 ---"
    tail -60 "$log" 2>/dev/null || true
    analyze_snapshot "$snap"
  } >"$snap/analysis.txt" 2>&1
  cat "$snap/analysis.txt" >&2
  echo "FAIL 现场已保留: $snap" >&2
}

diag_parallel() {
  [ "$DIAG" = 1 ] || return 0
  echo "### 诊断：cli 测试二进制 默认并行 vs --test-threads=1，各 3 次" >&2
  local mode i rc par_fail=0 ser_fail=0
  for mode in parallel serial; do
    for i in 1 2 3; do
      local log="$OUT/diag-$mode-$i.log" extra=()
      [ "$mode" = serial ] && extra=(-- --test-threads=1)
      if cargo test -p ui-cli --test cli --quiet "${extra[@]}" >"$log" 2>&1; then
        rc=0
      else
        rc=$?
      fi
      if [ "$rc" -eq 0 ]; then
        echo "  $mode $i: ok" >&2
      else
        echo "  $mode $i: FAIL rc=$rc log=$log" >&2
        if [ "$mode" = parallel ]; then par_fail=$((par_fail + 1)); else ser_fail=$((ser_fail + 1)); fi
      fi
    done
  done
  echo "  并行失败 $par_fail/3，串行失败 $ser_fail/3" >&2
  if [ "$par_fail" -gt 0 ] && [ "$ser_fail" -eq 0 ]; then
    echo "  => 指向并行/共享资源竞争，而不是断言本身。" >&2
  elif [ "$ser_fail" -gt 0 ]; then
    echo "  => 串行也失败：更像文件时序/环境，不是并行竞争。" >&2
  fi
}

# ── 单测 / 全量 ─────────────────────────────────────────────────────
single_case() {
  local idx=$1
  local log="$OUT/single-$idx.log"
  local t0 t1 rc=0
  t0=$(now_ms)
  cargo test -p ui-cli --test cli "$TEST" --quiet >"$log" 2>&1 || rc=$?
  t1=$(now_ms)
  if [ "$rc" -eq 0 ]; then
    printf 'ok   single %s/%s (%sms)\n' "$idx" "$N" "$((t1 - t0))"
    return 0
  fi
  printf 'FAIL single %s/%s rc=%s (%sms) log=%s\n' "$idx" "$N" "$rc" "$((t1 - t0))" "$log" >&2
  return 1
}

full_case() {
  local idx=$1
  local log="$OUT/full-$idx.log"
  local t0 t1 rc=0
  t0=$(now_ms)
  cargo test --workspace --quiet >"$log" 2>&1 || rc=$?
  t1=$(now_ms)
  if [ "$rc" -eq 0 ]; then
    printf 'ok   full   %s/%s (%sms)\n' "$idx" "$FULL" "$((t1 - t0))"
    return 0
  fi
  printf 'FAIL full %s/%s rc=%s (%sms) log=%s\n' "$idx" "$FULL" "$rc" "$((t1 - t0))" "$log" >&2
  return 1
}

echo "flake loop: single=$N full=$FULL out=$OUT"
i=0
for ((i = 1; i <= N; i++)); do
  if ! single_case "$i"; then
    FAILS=$((FAILS + 1))
    FAIL_LIST="$FAIL_LIST single:$i"
    snapshot_failure single "$i"
  fi
done
for ((i = 1; i <= FULL; i++)); do
  if ! full_case "$i"; then
    FAILS=$((FAILS + 1))
    FAIL_LIST="$FAIL_LIST full:$i"
    snapshot_failure full "$i"
  fi
done

{
  echo "single_iters=$N full_iters=$FULL failures=$FAILS"
  echo "failed=$FAIL_LIST"
  echo "logs=$OUT"
} | tee "$OUT/summary.txt"

if [ "$FAILS" -gt 0 ]; then
  diag_parallel
  echo "FAIL flake loop: failures=$FAILS ($FAIL_LIST) 现场: $OUT" >&2
  exit 1
fi
echo "PASS flake loop: single=$N/20 full=$FULL logs=$OUT"
