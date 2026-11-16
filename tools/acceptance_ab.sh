#!/usr/bin/env bash
# A/B：同一个注入、同一个任务，只改「工具输出是否落盘」。
#   arm A（control）：--tool-chars 0   → 全部常驻上下文（旧行为）
#   arm B（treatment）：--tool-chars 2000 → 超长落盘，上下文只留首尾+指针
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"
BIN="$HOME/ui/target/release/ui"
TASK="$PWD/fixtures/acceptance-task2.txt"
BASE="$PWD/out/acceptance/ab"

bash tools/dev.sh build --release 2>&1 | tail -1

run_arm() {
  local label="$1" chars="$2"
  local ws="$BASE/$label/repo"
  bash tools/_node.sh tools/acceptance_prepare2.js "$ws"

  local start end code
  start=$(date +%s)
  timeout 900 "$BIN" run --task-file "$TASK" --workspace "$ws" --yes \
    --max-calls 25 --max-tokens 300000 --tool-chars "$chars" > "$BASE/$label/run.log" 2>&1
  code=$?
  end=$(date +%s)

  local tokens steps calls green reason
  tokens=$(grep -oE '约 [0-9]+ tokens' "$BASE/$label/run.log" | tail -1 | grep -oE '[0-9]+')
  steps=$(grep -oE '[0-9]+ 步' "$BASE/$label/run.log" | tail -1 | grep -oE '[0-9]+')
  calls=$(grep -oE '[0-9]+ 次工具' "$BASE/$label/run.log" | tail -1 | grep -oE '[0-9]+')
  reason=$(grep -oE '结束原因 [a-z_]+' "$BASE/$label/run.log" | tail -1 | awk '{print $2}')
  if ( cd "$ws" && cargo test --offline >/dev/null 2>&1 ); then green="绿"; else green="红"; fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$chars" "$tokens" "$steps" "$calls" "$((end-start))" "$green" "$reason"
}

mkdir -p "$BASE/a" "$BASE/b"
echo -e "arm\ttool_chars\ttokens\tsteps\tcalls\t秒\t测试\t结束"
run_arm a 0
run_arm b 2000

echo
echo "=== 它改了什么（arm b）==="
diff -u ui-core/src/tools.rs "$BASE/b/repo/ui-core/src/tools.rs" | head -20 || true

# 副本里的 cargo 产物几百 MB，日志不需要它
find "$BASE" -maxdepth 5 -type d -name target -prune -exec rm -rf {} + 2>/dev/null
