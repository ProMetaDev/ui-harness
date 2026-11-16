#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 静态守卫：空状态的判定必须看「渲染出来的消息」，不是事件条数。
# 背景：曾经用 `__UI_BOOT__.some(e => e.type !== 'session')` 判断，
#       把 lab-run 这类元事件算成内容，结果欢迎页被隐藏、主区白屏。
set -uo pipefail
cd "$UI_ROOT"
HTML=ui-cli/assets/chat.html

fail() { echo "FAIL: $1"; exit 1; }

grep -q 'log.children.length' "$HTML" || fail "空状态没有按渲染行数判断"
grep -q '\.empty\.show' "$HTML" || fail "缺少 .empty.show 显示规则"

if grep -q "__UI_BOOT__.some" "$HTML"; then
  fail "还在用事件条数判断是否有内容（元事件会导致白屏）"
fi

# 侧栏三个视图 + 实验室的接口都应存在
for needle in "/api/lab/bundles" "/api/lab/run" "/api/lab/report"; do
  grep -q "$needle" ui-cli/src/server.rs || fail "服务端缺少 $needle"
done

echo "PASS 空状态守卫 + 实验室接口齐全"
