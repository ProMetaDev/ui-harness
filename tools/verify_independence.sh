#!/usr/bin/env bash
# 独立性门禁：核心不得悄悄引入第三方 crate / CDN / npm 运行时依赖。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

fail() { echo "FAIL: $1"; exit 1; }

[ -f Cargo.lock ] || fail "缺少 Cargo.lock"
[ -f docs/38-independence.md ] || fail "缺少 docs/38-independence.md"

names=$(grep '^name = ' Cargo.lock | sed 's/^name = "//; s/"$//' | sort -u)
expected=$'ui-cli\nui-core'
if [ "$names" != "$expected" ]; then
  echo "实际 Cargo.lock 包："
  echo "$names"
  fail "Cargo.lock 出现了第三方包（L0 只允许 ui-core / ui-cli）"
fi

# ui-core 不应有 [dependencies]
if awk '/^\[dependencies\]/{found=1} END{exit found?0:1}' ui-core/Cargo.toml; then
  fail "ui-core 出现了 [dependencies]"
fi

# ui-cli 只允许 path 依赖 ui-core
awk '/^\[dependencies\]/{f=1;next} /^\[/{f=0} f && NF {print}' ui-cli/Cargo.toml \
  | grep -vE '^ui-core[[:space:]]*=[[:space:]]*\{[[:space:]]*path[[:space:]]*=' \
  | grep -v '^[[:space:]]*$' \
  && fail "ui-cli 出现了非 path 的第三方依赖"

# 前端资源不允许外部 src/href（CDN）
if grep -RInE "(src|href)[[:space:]]*=[[:space:]]*[\"']https?://" ui-cli/assets >/tmp/ui-cdn.txt 2>/dev/null; then
  cat /tmp/ui-cdn.txt
  fail "Web 资源引用了外部 CDN"
fi

if git ls-files | grep -qE '(^|/)(package\.json|node_modules/)'; then
  fail "仓库跟踪了 npm 运行时文件"
fi

for marker in 'L0 内核层' 'L1 可选 OS 工具' 'L2 可选 skill / 企业 adapter'; do
  grep -qF "$marker" docs/38-independence.md || fail "独立性文档缺少 $marker"
done

echo "PASS 独立性：核心 std-only、无 CDN、无 npm 运行时"
