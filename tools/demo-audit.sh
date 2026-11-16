#!/usr/bin/env bash
# 一次完整的审计体验：不用模型、不用网络，30 秒跑完。
#
# 六步：跑一轮 → 回放 → 校验 → 篡改 → 发现 → 修复
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=${WS:-/tmp/ui-demo-audit}
rm -rf "$WS"; mkdir -p "$WS"

step() { printf '\n\033[1m── %s ──\033[0m\n' "$1"; }
cmd()  { printf '\033[2m$ %s\033[0m\n' "$1"; }

# ── 1. 跑一轮：让 agent 干两件事 ──
step "1. 跑一轮（剧本驱动，不花钱）"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"用 shell 生成报告","toolCalls":[{"name":"shell","args":{"command":"echo 'acc=0.96' > report.txt"}}]},
  {"content":"再精确写一个配置","toolCalls":[{"name":"fs_write","args":{"path":"config.json","content":"{\"seed\":1}"}}]},
  {"content":"完成"}]}
JSON
cmd "ui run --task demo --mock sc.json --workspace $WS --yes"
"$UI_BIN" run --task demo --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1 | grep -E '模型|→|结束原因' | head -8

# ── 2. 回放：看到"实际改了什么" ──
step "2. 回放（注意 effect/observed 那一行）"
cmd "ui replay --workspace $WS"
"$UI_BIN" replay --workspace "$WS" 2>&1 | grep -vE '^\s+(core|env|tools|history)\s' | head -14

# ── 3. 校验：链是好的 ──
step "3. 校验（干净的日志）"
cmd "ui verify --workspace $WS"
"$UI_BIN" verify --workspace "$WS" 2>&1 | tail -4
echo "退出码: $?"

# ── 4. 篡改：改一个字符 ──
step "4. 篡改一个字符"
cmd "sed -i 's/acc=0.96/acc=0.99/' $WS/.ui/journal.jsonl"
sed -i 's/acc=0.96/acc=0.99/' "$WS/.ui/journal.jsonl"
echo "（把报告里的数字从 0.96 改成 0.99——这正是"事后美化数据"的样子）"

# ── 5. 发现：两套校验器都能抓到 ──
step "5. 发现（我们的 Rust 校验器 + 独立 Python 校验器）"
cmd "ui verify --workspace $WS"
"$UI_BIN" verify --workspace "$WS" 2>&1 | tail -3
echo "ui verify 退出码: $?"
cmd "python3 tools/verify_journal.py $WS/.ui/journal.jsonl"
python3 tools/verify_journal.py "$WS/.ui/journal.jsonl"
echo "python 退出码: $?"

# ── 6. 修复：截断尾部是可恢复的，"被改"不是 ──
step "6. 对照：截断尾部可以修复，被改不能"
cp "$WS/.ui/journal.jsonl" /tmp/ui-demo-tampered.jsonl
rm -rf "$WS/.ui"
"$UI_BIN" run --task demo --mock "$WS/sc.json" --workspace "$WS" --yes >/dev/null 2>&1
size=$(stat -c%s "$WS/.ui/journal.jsonl")
head -c $((size - 30)) "$WS/.ui/journal.jsonl" > "$WS/.ui/torn.jsonl"
cmd "python3 tools/verify_journal.py <被截断的日志>"
python3 tools/verify_journal.py "$WS/.ui/torn.jsonl"
echo "python 退出码: $?（3 = 尾部残缺，可修复）"
cmd "ui verify --journal $WS/.ui/torn.jsonl"
"$UI_BIN" verify --workspace "$WS" --journal "$WS/.ui/torn.jsonl" 2>&1 | tail -3
echo "ui verify 退出码: $?（可修复，不是"被改"）"

# 修复本身也要留痕：再跑一轮时 open 会截掉撕裂尾巴并记一条 journal/torn
cmd "ui run ...（open 时修复，并记一笔 journal/torn）"
"$UI_BIN" run --task demo --mock "$WS/sc.json" --workspace "$WS" --journal "$WS/.ui/torn.jsonl" --yes >/dev/null 2>&1
cmd "ui replay --journal $WS/.ui/torn.jsonl | grep torn"
"$UI_BIN" replay --workspace "$WS" --journal "$WS/.ui/torn.jsonl" 2>&1 | grep -E 'journal/torn|崩溃' | head -2

printf '\n\033[1m结论\033[0m：改过的记录改不回来（两套校验器都抓得到）；\n'
printf '        崩溃撕掉的尾巴可以修复，但"修复过"这件事本身也会被写进日志。\n'
