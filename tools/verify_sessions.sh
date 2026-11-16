#!/usr/bin/env bash
# 会话：新建 / 列 / 用 --session 写入互不干扰 / 默认会话仍可用
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
WS=/tmp/ui-sessions
rm -rf "$WS"; mkdir -p "$WS"

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[{"content":"在会话里说一句"}]}
EOF

fail() { echo "FAIL: $1"; exit 1; }

echo "=== 新建两个会话 ==="
"$BIN" sessions --workspace "$WS" --new work
"$BIN" sessions --workspace "$WS" --new experiment
echo "PASS 新建"

echo
echo "=== 重名应该被拒 ==="
out=$("$BIN" sessions --workspace "$WS" --new work 2>&1 || true)
echo "$out" | grep -q '已经存在' || fail "重名没被拒：$out"
echo "PASS 重名拒绝"

echo
echo "=== 非法名字应该被拒 ==="
out=$("$BIN" sessions --workspace "$WS" --new '../evil' 2>&1 || true)
echo "$out" | grep -q '只能用字母' || fail "非法名字没被拒：$out"
echo "PASS 非法名字拒绝"

echo
echo "=== 往不同会话各跑一轮 ==="
"$BIN" run --task "a" --mock "$WS/sc.json" --workspace "$WS" --yes --session work >/dev/null 2>&1
"$BIN" run --task "b" --mock "$WS/sc.json" --workspace "$WS" --yes --session experiment >/dev/null 2>&1
"$BIN" run --task "c" --mock "$WS/sc.json" --workspace "$WS" --yes >/dev/null 2>&1

w=$(grep -c . "$WS/.ui/sessions/work.jsonl" 2>/dev/null || echo 0)
e=$(grep -c . "$WS/.ui/sessions/experiment.jsonl" 2>/dev/null || echo 0)
d=$(grep -c . "$WS/.ui/journal.jsonl" 2>/dev/null || echo 0)
echo "work=$w experiment=$e default=$d"
[ "$w" -gt 0 ] && [ "$e" -gt 0 ] && [ "$d" -gt 0 ] || fail "三个日志没有各自写入"
grep -q '在会话里说一句' "$WS/.ui/sessions/work.jsonl" || fail "work 里没有这轮内容"
echo "PASS 三个会话互不干扰"

echo
echo "=== 列表 ==="
"$BIN" sessions --workspace "$WS" | tee "$WS/list.txt"
grep -q 'work' "$WS/list.txt" || fail "列表没有 work"
grep -q 'experiment' "$WS/list.txt" || fail "列表没有 experiment"
grep -q 'default' "$WS/list.txt" || fail "列表没有 default"
echo "PASS 列表"

echo
echo "=== 每个会话的哈希链都完整 ==="
for f in "$WS/.ui/sessions/work.jsonl" "$WS/.ui/sessions/experiment.jsonl" "$WS/.ui/journal.jsonl"; do
  "$BIN" verify --workspace "$WS" --journal "$f" 2>&1 | grep -q '哈希链完整' || fail "$f 链坏了"
done
echo "PASS 三个日志链都完整"
