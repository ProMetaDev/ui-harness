#!/usr/bin/env bash
# 重新生成 docs/23 的「硬指标」表与快照元信息。
#
# 以前这里靠一个 old_table 字符串 assert：文档一改，脚本就崩，状态页只能手改并继续漂。
# 现在按「# 完成度快照」标题和「## 硬指标」小节定位并整段替换，不依赖旧表内容；
# 文档其余部分（分项完成度、仍未解决等人工判断）原样保留。
#
# 用法：
#   bash tools/status_doc.sh
#   STATUS_TESTS=399 bash tools/status_doc.sh    # 已有测试结果时跳过 cargo
#   STATUS_DATE=2026-11-27 bash tools/status_doc.sh
#
# 数字来源：tools/count_tests.sh（cargo test 汇总）、tools/debug.sh 的 run 行、
# docs/*.md 计数、源码/测试行数与 git。任何一项取不到都不会让脚本崩：
# 测试数会沿用上一次快照并在文档里明确标注，git 不可用时写 "?"。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# WSL 里 worktree 的 .git 可能写着 Windows 路径（gitdir: X:/...），git 读不懂。
# 显式把它翻成 GIT_DIR/GIT_WORK_TREE；普通 checkout 上 git 正常，自动跳过。
if ! git rev-parse --git-dir >/dev/null 2>&1 && [ -f .git ] && command -v wslpath >/dev/null 2>&1; then
  gitdir=$(sed -n 's/^gitdir:[[:space:]]*//p' .git | head -1)
  if [ -n "$gitdir" ]; then
    export GIT_DIR="$(wslpath -u "$gitdir")"
    export GIT_WORK_TREE="$UI_ROOT"
  fi
fi

is_num() {
  case "${1:-}" in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

DATE="${STATUS_DATE:-$(date +%F)}"
VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || VERSION="unknown"

GATES=$(grep -c '^run ' tools/debug.sh 2>/dev/null)
is_num "$GATES" || GATES=0
DOCS=$(find docs -maxdepth 1 -type f -name '*.md' | wc -l | tr -d ' ')
CORE=$(cat ui-core/src/*.rs 2>/dev/null | wc -l | tr -d ' ')
CLI=$(cat ui-cli/src/*.rs 2>/dev/null | wc -l | tr -d ' ')
TL=$(cat ui-core/tests/*.rs ui-cli/tests/*.rs 2>/dev/null | wc -l | tr -d ' ')
COMMITS=$(git rev-list --count HEAD 2>/dev/null)
is_num "$COMMITS" || COMMITS="?"
TAGS=$(git tag 2>/dev/null | wc -l | tr -d ' ')
is_num "$TAGS" || TAGS="?"
LATEST=$(git describe --tags --abbrev=0 2>/dev/null || printf '无')

# 测试数：显式覆盖 > 现跑 cargo > 沿用快照。cargo 缺失时不让脚本失败。
CAVEAT=""
if is_num "${STATUS_TESTS:-}"; then
  TESTS="$STATUS_TESTS"
elif TESTS=$(bash tools/count_tests.sh 2>/dev/null | tail -1 | awk '{print $3}') && is_num "$TESTS"; then
  :
else
  TESTS=$(sed -n 's/^| 测试 | \*\*\([0-9][0-9]*\) 个\*\*.*/\1/p' docs/23-status.md | head -1)
  is_num "$TESTS" || TESTS=0
  CAVEAT='本机未能重跑 `tools/count_tests.sh`，测试数沿用上一次快照；要刷新当前值请在有 cargo 的环境重跑本脚本。'
fi

python3 - "$DATE" "$VERSION" "$TESTS" "$CORE" "$CLI" "$TL" "$GATES" "$DOCS" "$COMMITS" "$TAGS" "$LATEST" "$CAVEAT" <<'PY'
import io
import re
import sys

(date, version, tests, core, cli, tl, gates, docs, commits, tags, latest, caveat) = sys.argv[1:13]

p = 'docs/23-status.md'
t = io.open(p, encoding='utf-8').read()
orig = t

note = (
    '> 本文档的**硬指标**由 `bash tools/status_doc.sh` 生成'
    f'（{date} 快照 · {version}），不要手改；\n'
    '> 分项完成度与「仍未解决」是人工判断，不由脚本生成。\n'
    '> 数字来源：`tools/count_tests.sh`、`tools/debug.sh` 的 `run` 行、`docs/*.md` 计数与 git。\n'
)
if caveat:
    note += f'> **注意**：{caveat}\n'

m = re.search(r'^## 硬指标[ \t]*$', t, re.M)
if not m:
    sys.stderr.write('docs/23 缺少「## 硬指标」小节，拒绝改写\n')
    raise SystemExit(2)

head = t[:m.start()]
title = re.match(r'^# 完成度快照[^\n]*\n', head)
if not title:
    sys.stderr.write('docs/23 缺少「# 完成度快照」标题，拒绝改写\n')
    raise SystemExit(2)

new_head = f'# 完成度快照（{date} 快照 · {version}）\n\n{note}\n'

# 硬指标小节从标题后到下一个二级标题（或文件末尾）整段替换，不依赖旧表字符串。
after = t[m.end():]
nxt = re.search(r'^## ', after, re.M)
body_end = m.end() + (nxt.start() if nxt else len(after))

table = (
    '| 指标 | 数值 |\n'
    '|---|---|\n'
    f'| Rust 代码 | **{core} 行** core + **{cli} 行** cli（零第三方依赖） |\n'
    f'| 测试 | **{tests} 个**（{tl} 行测试代码） |\n'
    f'| 门禁 | **{gates} 项**（`bash tools/debug.sh`） |\n'
    '| 工具 | 13 个 + MCP + 插件网关 |\n'
    f'| 文档 | **{docs} 篇**（另有 `tools/README.md` 索引） |\n'
    f'| 提交 / 标签 | {commits} 个 / {tags} 个标签（最新 `{latest}`） |\n'
    f'| CI | GitHub Actions 三个 job：`linux`（{gates} 项门禁，含文档一致性与隐私两档）'
    '+ `windows`（交叉编译）+ `windows-runtime`（Windows 实机进程树验证） |\n'
)

new = new_head + '## 硬指标\n\n' + table + '\n' + t[body_end:]

if new == orig:
    print('docs/23 无变化')
else:
    io.open(p, 'w', encoding='utf-8', newline='\n').write(new)
    print(f'docs/23 已更新：{date} · {version} · 测试 {tests} · 门禁 {gates} · 文档 {docs}')
PY
