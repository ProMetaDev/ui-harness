#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

python3 - <<'PY'
p = 'README.md'
t = open(p, encoding='utf-8').read()
t = t.replace('132 个测试', '137 个测试').replace('131 个测试', '137 个测试')
t = t.replace('132 个测试', '137 个测试')
open(p, 'w', encoding='utf-8').write(t)
print('README 测试数已更新')
PY

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'feat: observed effects for shell/plugin/mcp (effect/observed; UI_OBSERVE=0 to disable)'
git push origin main 2>&1 | tail -2
git log --oneline -1
echo "版本: $("$UI_BIN" --version)"
