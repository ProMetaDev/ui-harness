#!/usr/bin/env bash
# 浸泡测试：把"长任务可靠性"从主张变成数据。
#
# 两个轴都要测，因为它们暴露的问题不同：
#   A. 单进程长任务：一个 run 里几千次工具调用 —— 查内存泄漏、上下文压缩、预算。
#   B. 跨进程连续追加：反复跑同一个会话 —— 查哈希链、会话历史、日志增长。
#
# 用法：
#   bash tools/soak.sh                 # 默认 A:800 步 + B:10 分钟
#   bash tools/soak.sh --steps 2000 --minutes 60
#   bash tools/soak.sh --minutes 0     # 只跑 A
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

STEPS=800
MINUTES=10
SAMPLE_MS=200

while [ $# -gt 0 ]; do
  case "$1" in
    --steps) STEPS="${2:-800}"; shift 2 ;;
    --minutes) MINUTES="${2:-10}"; shift 2 ;;
    --sample-ms) SAMPLE_MS="${2:-200}"; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 1 ;;
  esac
done

ROOT=${SOAK_ROOT:-/tmp/ui-soak}
rm -rf "$ROOT"; mkdir -p "$ROOT"
FAILED=0
ROW_FILE="$ROOT/rows.txt"
: > "$ROW_FILE"

row() { printf '%-34s %s\n' "$1" "$2" >> "$ROW_FILE"; }
note() { printf '\n\033[1m── %s ──\033[0m\n' "$1"; }

# ────────────────────── A. 单进程长任务 ──────────────────────
note "A. 单进程长任务（$STEPS 次工具调用）"
WS_A="$ROOT/a"
mkdir -p "$WS_A"

python3 - "$WS_A/sc.json" "$STEPS" <<'PY'
import json, sys
path, n = sys.argv[1], int(sys.argv[2])
steps = []
for i in range(n):
    # 交替：小写入 + 读回；每 50 步做一次稍大的写入
    if i % 50 == 49:
        steps.append({"content": f"big {i}", "toolCalls": [
            {"name": "fs_write", "args": {"path": "big.txt", "content": "x" * 20000}}]})
    else:
        steps.append({"content": f"step {i}", "toolCalls": [
            {"name": "fs_write", "args": {"path": f"f{i % 7}.txt", "content": f"v{i}"}},
            {"name": "fs_read", "args": {"path": f"f{i % 7}.txt"}}]})
steps.append({"content": "done"})
json.dump({"model": "scripted", "steps": steps}, open(path, "w"), ensure_ascii=False)
PY

start_a=$(date +%s)
# 后台跑，前台采样 RSS（这是查内存泄漏的唯一办法：同一进程内观察增长）
"$UI_BIN" run --task "soak A" --mock "$WS_A/sc.json" --workspace "$WS_A" --yes \
  --max-calls $((STEPS * 3)) --max-steps $((STEPS * 2)) --max-tokens 20000000 \
  --max-history-tokens 2500 > "$ROOT/a.log" 2>&1 &
pid=$!
peak=0
samples=0
first=0
while kill -0 "$pid" 2>/dev/null; do
  rss=$(awk '/VmRSS/{print $2}' "/proc/$pid/status" 2>/dev/null || echo 0)
  rss=${rss:-0}
  [ "$first" -eq 0 ] && first=$rss
  [ "$rss" -gt "$peak" ] && peak=$rss
  samples=$((samples + 1))
  sleep "$(awk -v ms="$SAMPLE_MS" 'BEGIN{printf "%.3f", ms/1000}')"
done
wait "$pid"; code_a=$?
elapsed_a=$(( $(date +%s) - start_a ))

calls=$(grep -c 'tool/request' "$WS_A/.ui/journal.jsonl" 2>/dev/null); calls=${calls:-0}
events_a=$(wc -l < "$WS_A/.ui/journal.jsonl" 2>/dev/null); events_a=${events_a:-0}
compacts=$(grep -c '"kind":"compact"' "$WS_A/.ui/journal.jsonl" 2>/dev/null); compacts=${compacts:-0}
size_a=$(du -h "$WS_A/.ui/journal.jsonl" 2>/dev/null | cut -f1)

row "A 退出码" "$code_a"
row "A 工具调用数" "$calls"
row "A 事件数" "$events_a"
row "A 压缩次数" "$compacts"
row "A 耗时" "${elapsed_a}s"
row "A 起始 RSS" "${first} kB"
row "A 峰值 RSS" "${peak} kB（采样 ${samples} 次）"
row "A 日志大小" "$size_a"

# 增长比：峰值 / 起始。>2 就要怀疑泄漏
if [ "$first" -gt 0 ]; then
  ratio=$(awk -v a="$first" -v b="$peak" 'BEGIN{printf "%.2f", b/a}')
  row "A 内存增长比" "${ratio}x"
  if awk -v r="$ratio" 'BEGIN{exit (r >= 3.0)}'; then
    row "A 内存" "PASS（增长 ${ratio}x < 3x）"
  else
    row "A 内存" "FAIL（增长 ${ratio}x，疑似泄漏）"
    FAILED=$((FAILED + 1))
  fi
fi
# 压缩只在历史超过阈值时触发；步数太少时不该期待它发生，标 N/A 而不是 FAIL
if [ "${compacts:-0}" -gt 0 ]; then
  row "A 压缩生效" "PASS（${compacts} 次）"
elif [ "${STEPS}" -ge 200 ]; then
  row "A 压缩生效" "FAIL（${STEPS} 步还没触发压缩）"
  FAILED=$((FAILED + 1))
else
  row "A 压缩生效" "N/A（${STEPS} 步太少，未到阈值）"
fi
if [ "$code_a" -ne 0 ]; then
  row "A 结果" "FAIL（退出码 $code_a）"
  FAILED=$((FAILED + 1))
fi

"$UI_BIN" verify --workspace "$WS_A" >/dev/null 2>&1
if [ $? -eq 0 ]; then row "A 链校验" "PASS"; else row "A 链校验" "FAIL"; FAILED=$((FAILED + 1)); fi

# ────────────────────── B. 跨进程连续追加 ──────────────────────
if [ "$MINUTES" -gt 0 ]; then
  note "B. 跨进程连续追加（$MINUTES 分钟）"
  WS_B="$ROOT/b"
  mkdir -p "$WS_B"
  cat > "$WS_B/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"写","toolCalls":[{"name":"fs_write","args":{"path":"n.txt","content":"x"}}]},
  {"content":"读","toolCalls":[{"name":"fs_read","args":{"path":"n.txt"}}]},
  {"content":"grep","toolCalls":[{"name":"grep","args":{"pattern":"x"}}]},
  {"content":"shell","toolCalls":[{"name":"shell","args":{"command":"echo tick"}}]},
  {"content":"完成"}]}
JSON

  iter=0
  fail_b=0
  start_b=$(date +%s)
  limit=$((MINUTES * 60))
  while [ $(( $(date +%s) - start_b )) -lt "$limit" ]; do
    iter=$((iter + 1))
    if ! "$UI_BIN" run --task "soak B $iter" --mock "$WS_B/sc.json" \
        --workspace "$WS_B" --session soak --yes >/dev/null 2>&1; then
      fail_b=$((fail_b + 1))
    fi
  done
  elapsed_b=$(( $(date +%s) - start_b ))
  lines_b=$(wc -l < "$WS_B/.ui/sessions/soak.jsonl" 2>/dev/null); lines_b=${lines_b:-0}
  size_b=$(du -h "$WS_B/.ui/sessions/soak.jsonl" 2>/dev/null | cut -f1)

  row "B 轮数" "$iter"
  row "B 失败轮数" "$fail_b"
  row "B 耗时" "${elapsed_b}s"
  row "B 日志行数" "$lines_b"
  row "B 日志大小" "$size_b"
  row "B 每分钟轮数" "$(awk -v n="$iter" -v s="$elapsed_b" 'BEGIN{ if (s>0) printf "%.1f", n*60/s; else print "?"}')"
  [ "$fail_b" -eq 0 ] || FAILED=$((FAILED + 1))

  "$UI_BIN" verify --workspace "$WS_B" --session soak >/dev/null 2>&1
  if [ $? -eq 0 ]; then row "B 链校验" "PASS"; else row "B 链校验" "FAIL"; FAILED=$((FAILED + 1)); fi

  # 会话历史不能无限涨：检查最近一次快照的 token 数
  last_tok=$("$UI_BIN" replay --workspace "$WS_B" --session soak 2>/dev/null \
    | grep 'prompt/snapshot' | tail -1 | grep -oE '[0-9]+ tok' | head -1)
  row "B 最后一次快照" "${last_tok:-?}"
fi

# ────────────────────── 收尾 ──────────────────────
note "结果"
while IFS= read -r line; do echo "  $line"; done < "$ROW_FILE"

# 孤儿检查：soak 结束后不该还有我们起的进程
orphans=$(pgrep -f "ui run --task soak" | wc -l)
echo
echo "孤儿进程: $orphans"
[ "$orphans" -eq 0 ] || FAILED=$((FAILED + 1))

echo
if [ "$FAILED" -eq 0 ]; then
  echo "SOAK PASS"
  exit 0
fi
echo "SOAK FAIL（$FAILED 项）"
exit 1
