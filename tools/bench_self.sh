#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 用 headless-harness-bench 的 T1–T7 在自己身上跑一遍（本地、真实进程）。
# 每条 0–3 分，满分 21。评分标准按它的口径，判定依据是实际行为不是文档。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
BASE=/tmp/selfbench
rm -rf "$BASE"; mkdir -p "$BASE"
TOTAL=0

say() { printf '%s\n' "$*"; }
score() { # name points note
  TOTAL=$((TOTAL + $2))
  printf '  %-28s %s/3   %s\n' "$1" "$2" "$3"
}

mkws() { local d="$1"; rm -rf "$d"; mkdir -p "$d"; echo "$d"; }

# ── T1 boot-to-JSON：能不能非交互跑完并产出机器可读输出 ──
ws=$(mkws "$BASE/t1")
cat > "$ws/sc.json" <<'EOF'
{"model":"scripted","steps":[{"content":"一句话回答"}]}
EOF
out=$("$BIN" run --task "回一句话" --mock "$ws/sc.json" --workspace "$ws" --yes 2>&1)
code=$?
j="$ws/.ui/journal.jsonl"
t1=0
[ $code -eq 0 ] && t1=$((t1+1))
[ -s "$j" ] && t1=$((t1+1))
python3 -c "import json,sys; [json.loads(l) for l in open('$j')]" 2>/dev/null && t1=$((t1+1))
score "T1 boot-to-JSON" $t1 "退出码 $code · 日志 $(wc -l < "$j" 2>/dev/null || echo 0) 行"

# ── T2 structured-parse：能否解析出 assistant 文本 + token + 工具调用 ──
ws=$(mkws "$BASE/t2")
cat > "$ws/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"写文件","toolCalls":[{"name":"fs_write","args":{"path":"a.txt","content":"x"}}]},
  {"content":"做完了"}]}
EOF
"$BIN" run --task t --mock "$ws/sc.json" --workspace "$ws" --yes >/dev/null 2>&1
t2=$(python3 - "$ws/.ui/journal.jsonl" <<'PY'
import json, sys
kinds = [json.loads(l) for l in open(sys.argv[1], encoding="utf-8")]
got = 0
if any(e.get("kind") == "model/response" and e.get("content") for e in kinds): got += 1
if any(e.get("kind") == "tool/request" for e in kinds): got += 1
if any(e.get("kind") == "turn/end" and "steps" in e for e in kinds): got += 1
if any(e.get("kind") == "tool/end" for e in kinds): got += 1
print(4 if got == 4 else got)
PY
)
score "T2 structured-parse" $([ "$t2" -ge 3 ] && echo 3 || echo "$t2") "解析出 $t2/4 项（assistant/tool-call/usage/tool-end）"

# ── T3 tool allowlist：白名单外的工具必须真的被拒 ──
ws=$(mkws "$BASE/t3")
cat > "$ws/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"偷写","toolCalls":[{"name":"fs_write","args":{"path":"a.txt","content":"x"}}]},
  {"content":"完"}]}
EOF
"$BIN" run --task t --mock "$ws/sc.json" --workspace "$ws" --yes --tools fs_read >/dev/null 2>&1
t3=0
[ ! -f "$ws/a.txt" ] && t3=$((t3+1))
grep -q 'tool/end' "$ws/.ui/journal.jsonl" && grep -q '"ok":false' "$ws/.ui/journal.jsonl" && t3=$((t3+2))
score "T3 tool-allowlist" $t3 "白名单外文件未创建且被记失败"

# ── T4 system-prompt 注入：追加的指令必须进快照 ──
ws=$(mkws "$BASE/t4")
t4=0
"$BIN" prompt --workspace "$ws" --append-system-prompt "SENTINEL-T4" --full 2>/dev/null | grep -q 'SENTINEL-T4' && t4=$((t4+2))
"$BIN" prompt --workspace "$ws" --full 2>/dev/null | grep -q 'SENTINEL-T4' && t4=0
score "T4 sysprompt-inject" $t4 "静态路径验证（未走真模型，所以不给满分）"

# ── T5 env isolation：密钥类变量不能进子进程 ──
ws=$(mkws "$BASE/t5")
cat > "$ws/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"看环境","toolCalls":[{"name":"shell","args":{"command":"echo S=${MY_API_KEY:-none} P=${MY_PLAIN:-none} X=$([ -n \"$PATH\" ] && echo yes)"}}]},
  {"content":"完"}]}
EOF
out=$(MY_API_KEY=leak MY_PLAIN=ok "$BIN" run --task t --mock "$ws/sc.json" --workspace "$ws" --yes 2>&1)
t5=0
echo "$out" | grep -q 'S=none' && t5=$((t5+2))
echo "$out" | grep -q 'P=ok' && t5=$((t5+1))
score "T5 env-isolation" $t5 "密钥被剔除且普通变量保留"

# ── T6 cancellation：超时必须杀掉整棵树 ──
ws=$(mkws "$BASE/t6")
cat > "$ws/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"长任务","toolCalls":[{"name":"shell","args":{"command":"sleep 5; echo done","timeoutMs":400}}]},
  {"content":"完"}]}
EOF
start=$(date +%s)
"$BIN" run --task t --mock "$ws/sc.json" --workspace "$ws" --yes >/dev/null 2>&1
secs=$(( $(date +%s) - start ))
sleep 0.3
orphan=$(pgrep -f 'sleep 5' | wc -l)
t6=0
[ $secs -le 3 ] && t6=$((t6+2))
[ "$orphan" -eq 0 ] && t6=$((t6+1))
score "T6 cancel/no-orphan" $t6 "耗时 ${secs}s · 残留进程 $orphan"

# ── T7 错误可机读：模型进程失败必须非零退出且留痕 ──
ws=$(mkws "$BASE/t7")
printf '#!/bin/sh\nexit 1\n' > "$BASE/fail.sh"; chmod +x "$BASE/fail.sh"
out=$(UI_MODEL_CMD="sh $BASE/fail.sh" "$BIN" run --task t --workspace "$ws" --yes 2>&1)
code=$?
t7=0
[ $code -ne 0 ] && t7=$((t7+1))
grep -q '"kind":"model/error"' "$ws/.ui/journal.jsonl" 2>/dev/null && t7=$((t7+1))
echo "$out" | grep -qiE '失败|error' && t7=$((t7+1))
score "T7 error machine-readable" $t7 "退出码 $code · 日志有 model/error"

echo
echo "T1–T7 合计: $TOTAL/21"
echo "（对照：dsh 20/21，flue/pi 17/21，omp/crush/eve 14/21）"
