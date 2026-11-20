#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# 1) 把误提交的临时脚本移出版本库
git rm -q --cached tools/_v022.py tools/_commit_022.sh 2>/dev/null || true
rm -f tools/_v022.py tools/_commit_022.sh

# 2) 版本号文档（上一轮被 PowerShell 解析失败吞掉了）
python3 - <<'PY'
import io
p='README.md'
t=io.open(p,encoding='utf-8').read()
t=t.replace('> **当前版本 0.2.1：可用，但不是生产就绪。**','> **当前版本 0.2.2：可用，但不是生产就绪。**')
t=t.replace('ui 0.2.1 (commit 0000000, built 2026-11-18)','ui 0.2.2 (commit 0000000, built 2026-11-20)')
t=t.replace('- **0.2.1** · 测试与门禁由','- **0.2.2** · 测试与门禁由')
io.open(p,'w',encoding='utf-8').write(t)

p='docs/34-versioning.md'
t=io.open(p,encoding='utf-8').read()
t=t.replace('''0.2.1   ← 网页端收尾 + 聊天流离线校验进门禁（不改变对外契约）
        224 个测试 / 51 项门禁 / 12 个攻击场景''','''0.2.1   ← 网页端收尾 + 聊天流离线校验进门禁（不改变对外契约）
0.2.2   ← **网页端脱敏闭合**（SSE + /snapshot + 工具全文库）：隐私两档不再有缺口
        301 个测试 / 54 项门禁 / 13 个攻击场景''')
io.open(p,'w',encoding='utf-8').write(t)
print('README / docs-34 已更新到 0.2.2')
PY

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'chore: drop scratch drivers; docs say 0.2.2'
echo "提交: $(git rev-parse --short HEAD)"
git push origin main 2>&1 | tail -1
git status --porcelain | wc -l
