#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# tools/README.md 里补一行说明（保持索引准确）
python3 - <<'PY'
import io
p = 'tools/README.md'
t = io.open(p, encoding='utf-8').read()
if 'ci_mingw.sh' not in t:
    for line in t.splitlines():
        if 'CI' in line and '|' in line:
            t = t.replace(line, line + '\n| `ci_mingw.sh` | CI 专用：下载 posix 变体的 .deb 解包到 `~/mingw64`（可缓存），不 install |')
            break
    io.open(p, 'w', encoding='utf-8').write(t)
    print('tools/README.md 已补 ci_mingw.sh')
else:
    print('tools/README.md 已有')
PY

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'docs(tools): index ci_mingw.sh' 2>/dev/null || echo "（无改动）"
git push origin main 2>&1 | tail -1
echo "HEAD: $(git rev-parse --short HEAD)"
echo "工作区: $(git status --porcelain | wc -l)"
echo "v0.2.0 → $(git rev-list -n1 v0.2.0 | cut -c1-7)"
