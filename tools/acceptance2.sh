#!/usr/bin/env bash
# 第二例：没有红灯可依赖。注入一个测试抓不到的隐患（去掉原子写），看它能否靠读代码发现。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
export UI_MODEL_CMD="bash tools/_node.sh tools/deepseek.mjs"
BIN="$HOME/ui/target/release/ui"
WS=/tmp/acceptance/repo2

bash tools/_node.sh tools/acceptance_prepare2.js "$WS"

echo
echo "=== 裁判自检：测试应该是全绿的（没有红灯）==="
( cd "$WS" && cargo test --offline 2>&1 | grep -E 'test result: ' | head -8 )

echo
echo "=== 交给 harness（开放式任务，25 次调用上限）==="
START=$(date +%s)
timeout 900 "$BIN" run \
  --task-file fixtures/acceptance-task2.txt \
  --workspace "$WS" \
  --yes \
  --max-calls 25 \
  --max-tokens 200000
CODE=$?
END=$(date +%s)
echo "EXIT=$CODE  耗时=$((END-START))s"

echo
echo "=== 它到底改了什么 ==="
diff -u ui-core/src/tools.rs "$WS/ui-core/src/tools.rs" | head -40 || true

echo
echo "=== 复核：测试还绿吗 ==="
( cd "$WS" && cargo test --offline 2>&1 | grep -E 'test result: ok|FAILED' | head -8 )

echo
echo "=== 轨迹 ==="
"$BIN" export --workspace "$WS" --html /tmp/acceptance/trace2.html

# 副本里的 cargo 产物几百 MB，日志不需要它
find "$WS" -maxdepth 5 -type d -name target -prune -exec rm -rf {} + 2>/dev/null
