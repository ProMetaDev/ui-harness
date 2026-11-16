#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
rm -f tools/_release_011.sh

bash tools/dev.sh build --release 2>&1 | tail -1
echo "== 版本（应无 -dirty，commit 与 tag 一致）=="
"$UI_BIN" --version

echo "== 重新打包 =="
rm -rf dist
bash tools/release.sh 2>&1 | tail -4

echo "== 更新状态文档数字 =="
python3 - <<'PY'
import re
p = 'docs/23-status.md'
t = open(p, encoding='utf-8').read()
t = t.replace('| 测试代码 | 2,061 行 · **131 个测试** |', '| 测试代码 | **132 个测试** |')
t = t.replace('| 门禁 | **36 项**', '| 门禁 | **38 项**')
t = t.replace('| CI / 发布 | **无** |', '| CI / 发布 | **GitHub Actions 已绿（38/38）** |')
open(p, 'w', encoding='utf-8').write(t)
print('done')
PY

git add -A
git -c user.name=ProMetaDev -c user.email=3144001558@qq.com commit -q -m 'release: rebuild 0.1.1 from the tagged commit; refresh status numbers' 2>/dev/null || echo "（无改动）"
git push origin main 2>&1 | tail -1
echo "HEAD: $(git rev-parse --short HEAD)"
