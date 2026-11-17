#!/usr/bin/env bash
# 门禁：知识卡的来源绑定在真实 CLI 上有效。
#
#   UNBOUND（空 source_sha）：默认退 0，--strict 退 2
#   用 `ui knowledge emit` 让**产品自己**算出 source_sha，回填卡片 → --strict 退 0
#   真改被 touches 覆盖的源文件 → 退 2 且输出 STALE
#   卡片文件本身不能被 check 改写
#
# 这里刻意不自己复现哈希算法：手写一份"同算法"很容易和产品实现漂移
# （第一版就是这么错的——算出来的值与 CLI 不一致，测试反而在测我自己的实现）。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-verify-knowledge
rm -rf "$WS"; mkdir -p "$WS/src/pay" "$WS/docs/knowledge"
echo "refund_v1" > "$WS/src/pay/refund.rs"

cat > "$WS/docs/knowledge/pay.md" <<'MD'
---
id: mod-pay
title: 支付模块
source_sha:
generated_by_seq: 0
confirmed_by: human
touches: ["src/pay/**"]
---
退款只有一条路径：先锁单后打款。
MD

out=$("$UI_BIN" knowledge check --workspace "$WS" 2>&1); code=$?
[ "$code" -eq 0 ] || { echo "FAIL UNBOUND 默认应退出 0，实际 $code"; echo "$out"; exit 1; }
echo "$out" | grep -q "UNBOUND" || { echo "FAIL 应明示 UNBOUND"; exit 1; }
"$UI_BIN" knowledge check --workspace "$WS" --strict >/dev/null 2>&1
[ $? -eq 2 ] || { echo "FAIL --strict 下 UNBOUND 应退出 2"; exit 1; }
echo "✓ UNBOUND：默认 0 / --strict 2"

# 用产品自己的 bind 命令绑定（绝不自己复现哈希算法）
"$UI_BIN" knowledge bind mod-pay --workspace "$WS" >/dev/null 2>&1 || { echo "FAIL bind 失败"; exit 1; }
sha=$(grep -m1 '^source_sha:' "$WS/docs/knowledge/pay.md" | sed 's/^source_sha: *//')
[ -n "$sha" ] || { echo "FAIL bind 之后卡片里没有 source_sha"; exit 1; }
echo "产品算出的 source_sha: ${sha:0:16}…"

"$UI_BIN" knowledge check --workspace "$WS" --strict >/dev/null 2>&1
[ $? -eq 0 ] || { echo "FAIL 绑定正确时 --strict 应退出 0"; "$UI_BIN" knowledge check --workspace "$WS" --strict; exit 1; }
echo "✓ 绑定正确：--strict 0"

echo "refund_v2" > "$WS/src/pay/refund.rs"
out=$("$UI_BIN" knowledge check --workspace "$WS" --strict 2>&1); code=$?
[ "$code" -eq 2 ] || { echo "FAIL 源文件变了应退出 2，实际 $code"; echo "$out"; exit 1; }
echo "$out" | grep -q "STALE" || { echo "FAIL 应输出 STALE"; exit 1; }
echo "✓ 源文件变了：STALE / 2"

grep -q "source_sha: $sha" "$WS/docs/knowledge/pay.md" || { echo "FAIL 卡片被改写了"; exit 1; }

echo "PASS 知识卡来源绑定有效（UNBOUND→0/2 · 绑定→0 · STALE→2 · 卡片不被改写）"
