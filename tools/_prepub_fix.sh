#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

python3 - <<'PY'
p = 'README.md'
t = open(p, encoding='utf-8').read()
t = t.replace('当前 v0.1.1', '当前 v0.1.2')
t = t.replace('原型，v0.1.1。', '原型，v0.1.2。')
# 公开前补一句"欢迎来看"，明确邀请外部验证
if '验证方式' in t and '欢迎用这些材料挑毛病' not in t:
    t = t.replace(
        '- 如果你跑出表外的结论，那比表内全部通过更有价值。',
        '- 如果你跑出表外的结论，那比表内全部通过更有价值。\n- **欢迎用这些材料挑毛病**：这个仓库的主要目的是被验证，不是被夸奖。\n  安全问题的报告方式见 [SECURITY.md](SECURITY.md)。'
    )
open(p, 'w', encoding='utf-8').write(t)
print('README 已更新')
PY

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'docs: pre-public pass (SECURITY.md, sanitize personal path, version bump)'
git push origin main 2>&1 | tail -2
echo "HEAD: $(git rev-parse --short HEAD)"
