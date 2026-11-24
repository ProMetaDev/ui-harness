#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

TESTS=$(bash tools/count_tests.sh | tail -1 | awk '{print $3}')
GATES=$(grep -c '^run ' tools/debug.sh)
CORE=$(cat ui-core/src/*.rs | wc -l)
CLI=$(cat ui-cli/src/*.rs | wc -l)
TL=$(cat ui-core/tests/*.rs ui-cli/tests/*.rs 2>/dev/null | wc -l)
DOCS=$(ls docs/*.md | wc -l)
COMMITS=$(git rev-list --count HEAD)
TAGS=$(git tag | wc -l)

python3 - "$TESTS" "$GATES" "$CORE" "$CLI" "$TL" "$DOCS" "$COMMITS" "$TAGS" <<'PY'
import io, sys
tests, gates, core, cli, tl, docs, commits, tags = sys.argv[1:9]
p = 'docs/23-status.md'
t = io.open(p, encoding='utf-8').read()

t = __import__('re').sub(
    r'^# 完成度快照.*$',
    '# 完成度快照（2026-11-24 · 0.3.0）\n\n'
    '> 本文件的**数字由脚本生成**（见 `tools/status_doc.sh`），不要手改；\n'
    '> 手改一定会漂——上一版这里写着 133 个测试 / 38 项门禁，实际已经是 374 / 57。',
    t, count=1, flags=__import__('re').M)

old_table = '''| 指标 | 数值 |
|---|---|
| Rust 代码 | **13,930 行**（零第三方依赖） |
| 测试 | **133 个**（2,146 行测试代码） |
| 门禁 | **38 项**（Linux CI 每次跑满） |
| 工具 | 13 个 + MCP + 插件网关 |
| 文档 / 脚本 | 25 篇 / 104 个 |
| 二进制 | Linux **1.9 MB** / Windows **2.1 MB** |
| 提交 / 标签 | 47 个 / `v0.1.0`（未绿）、`v0.1.1`（首个全绿） |
| CI | **GitHub Actions 绿**：`linux`（38 项门禁）+ `windows`（交叉编译） |'''

new_table = f'''| 指标 | 数值 |
|---|---|
| Rust 代码 | **{core} 行** core + **{cli} 行** cli（零第三方依赖） |
| 测试 | **{tests} 个**（{tl} 行测试代码） |
| 门禁 | **{gates} 项**（Linux CI 每次跑满） |
| 工具 | 13 个 + MCP + 插件网关 |
| 文档 | **{docs} 篇**（另有 `tools/README.md` 索引） |
| 提交 / 标签 | {commits} 个 / {tags} 个标签（最新 `v0.2.2`） |
| CI | **GitHub Actions 绿**：`linux`（{gates} 项门禁，含文档一致性与隐私两档）+ `windows`（交叉编译，0.7 分钟） |'''

assert old_table in t
t = t.replace(old_table, new_table)

t = t.replace('| **隐私与落账** | 落账前脱敏门 ＋ `--private` 来源扣留 ＋ 两个落盘目录（0600/0700）＋ 保留期与 run 边界自动清 ＋ `ui spill`/`ui secrets`/`doctor` 三处可见；缺读路径拦截与网页端脱敏 | **78%** |',
              '| **隐私与落账** | 落账前脱敏门 ＋ 来源扣留 ＋ 两个落盘目录（0600/0700）＋ 保留期与 run 边界自动清 ＋ **网页端同门脱敏**（SSE/快照/工具全文库）＋ `ui spill`/`ui secrets`/`doctor` 三处可见；缺读路径拦截 | **88%** |')

io.open(p, 'w', encoding='utf-8').write(t)
print('docs/23 已按脚本数字更新')
PY
