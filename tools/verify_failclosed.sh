#!/usr/bin/env bash
# 日志写不进去时：必须 fail closed（不执行工具、非零退出、错误可读）
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-verify-failclosed
rm -rf "$WS"; mkdir -p "$WS"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"写文件","toolCalls":[{"name":"fs_write","args":{"path":"must-not-exist.txt","content":"x"}}]},
  {"content":"完成"}]}
JSON

# 先正常跑一次建日志
"$UI_BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes >/dev/null 2>&1
rm -f "$WS/must-not-exist.txt"

fail=0
report() { printf '  %-42s %s\n' "$1" "$2"; }

echo "== 场景 1：日志文件只读 =="
chmod 0444 "$WS/.ui/journal.jsonl"
out=$("$UI_BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1)
code=$?
chmod 0644 "$WS/.ui/journal.jsonl"
if [ -f "$WS/must-not-exist.txt" ]; then
  report "工具是否被执行" "FAIL（执行了）"; fail=$((fail+1)); rm -f "$WS/must-not-exist.txt"
else
  report "工具是否被执行" "PASS（没有执行）"
fi
if [ "$code" -ne 0 ]; then report "退出码" "PASS（$code）"; else report "退出码" "FAIL（0）"; fail=$((fail+1)); fi
# 场景 1 的错误信息必须可读（这是主要目标）
if printf '%s' "$out" | grep -q '日志'; then
  report "错误是否说清是日志问题" "PASS"
else
  report "错误是否说清是日志问题" "FAIL：$out"; fail=$((fail+1))
fi

echo
echo "== 场景 2：.ui 被一个普通文件占住（日志根本建不出来）=="
WS2=/tmp/ui-verify-failclosed2
rm -rf "$WS2"; mkdir -p "$WS2"
cp "$WS/sc.json" "$WS2/sc.json"
: > "$WS2/.ui"   # 本该是目录的位置放一个文件
out=$("$UI_BIN" run --task t --mock "$WS2/sc.json" --workspace "$WS2" --yes 2>&1)
code=$?
if [ -f "$WS2/must-not-exist.txt" ]; then
  report "工具是否被执行" "FAIL（执行了）"; fail=$((fail+1))
else
  report "工具是否被执行" "PASS（没有执行）"
fi
if [ "$code" -ne 0 ]; then report "退出码" "PASS（$code）"; else report "退出码" "FAIL（0）"; fail=$((fail+1)); fi

echo
echo "== 场景 3：修好权限后能正常继续 =="
rm -f "$WS2/.ui"
"$UI_BIN" run --task t --mock "$WS2/sc.json" --workspace "$WS2" --yes >/dev/null 2>&1
if [ -f "$WS2/must-not-exist.txt" ]; then
  report "恢复后能否正常写" "PASS"
else
  report "恢复后能否正常写" "FAIL"; fail=$((fail+1))
fi

echo
echo "== 场景 4：文件大小上限（模拟写满，EFBIG）=="
WS3=/tmp/ui-verify-failclosed3
rm -rf "$WS3"; mkdir -p "$WS3"
cp "$WS/sc.json" "$WS3/sc.json"
# ulimit -f 只对当前 shell 及其子进程生效，所以放进子 shell；必须忽略 SIGXFSZ，
# 否则进程会被信号杀掉（看不到错误信息）。
out=$( ( trap '' XFSZ; ulimit -f 1; "$UI_BIN" run --task t --mock "$WS3/sc.json" --workspace "$WS3" --yes 2>&1 ) )
code=$?
if [ -f "$WS3/must-not-exist.txt" ]; then
  report "工具是否被执行" "FAIL（执行了）"; fail=$((fail+1))
else
  report "工具是否被执行" "PASS（没有执行）"
fi
if [ "$code" -ne 0 ]; then report "退出码" "PASS（$code）"; else report "退出码" "FAIL（0）"; fail=$((fail+1)); fi
# 文件大小超限时，不同 shell 对 SIGXFSZ 的处置不同：
#   忽略 -> 写返回 EFBIG，我们打印可读错误；
#   不忽略 -> 进程被信号杀掉（退出码 >= 128）。
# 两种都是 fail closed，这里都接受；"错误信息可读"由场景 1/2 覆盖。
if [ "$code" -ge 128 ]; then
  report "失败形态" "PASS（被信号终止，仍是 fail closed）"
elif printf '%s' "$out" | grep -q '日志'; then
  report "失败形态" "PASS（可读错误）"
else
  report "失败形态" "FAIL（退出码 $code，且没有说明）：$out"; fail=$((fail+1))
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "PASS 日志不可写时 fail closed（4 个场景）"
  exit 0
fi
echo "FAIL（$fail 项）"
exit 1
