#!/usr/bin/env bash
# 真实验收（第一例：有红灯）。产物落在 out/acceptance/ 下，WSL 重启也不会丢。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"
BIN="$HOME/ui/target/release/ui"
BASE="$PWD/out/acceptance/case1"
WS="$BASE/repo"
TASK="$PWD/fixtures/acceptance-task.txt"

bash tools/dev.sh build --release 2>&1 | tail -1
bash tools/_node.sh tools/acceptance_prepare.js "$WS"

count_tests() { ( cd "$WS" && grep -rho '#\[test\]' --include='*.rs' . | wc -l | tr -d ' ' ); }
count_ignored() { ( cd "$WS" && grep -rho '#\[ignore\]' --include='*.rs' . | wc -l | tr -d ' ' ); }
TESTS_BEFORE=$(count_tests)
IGNORED_BEFORE=$(count_ignored)

echo
echo "=== 裁判先自检：现在应该是红的 ==="
( cd "$WS" && cargo test --offline 2>&1 | grep -E '^test .* FAILED' | head -3 )
echo "#[test] 总数: $TESTS_BEFORE  ·  #[ignore]: $IGNORED_BEFORE"

echo
echo "=== 交给 harness（真模型，最多 30 次调用）==="
START=$(date +%s)
timeout 1200 "$BIN" run --task-file "$TASK" --workspace "$WS" --yes \
  --max-calls 30 --max-tokens 200000 --max-wall-ms 1200000
CODE=$?
END=$(date +%s)
echo "EXIT=$CODE  耗时=$((END-START))s"

echo
echo "=== 裁判复核 ==="
( cd "$WS" && cargo test --offline 2>&1 | grep -E 'test result' )
echo "#[test] 总数: $TESTS_BEFORE → $(count_tests)  ·  #[ignore]: $IGNORED_BEFORE → $(count_ignored)"

echo
echo "=== 它改了什么 ==="
diff -u ui-core/src/policy.rs "$WS/ui-core/src/policy.rs" | head -30 || true

echo
echo "=== 轨迹（持久目录）==="
"$BIN" export --workspace "$WS" --html "$BASE/trace.html"

# 副本里的 cargo 产物几百 MB，日志不需要它
find "$BASE" -maxdepth 5 -type d -name target -prune -exec rm -rf {} + 2>/dev/null
