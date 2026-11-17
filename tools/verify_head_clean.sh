#!/usr/bin/env bash
# 干净检出验证：从 HEAD 建一个临时 worktree 并构建 + 跑知识测试。
#
# 为什么需要：工作树是绿的，不代表**提交**是绿的。曾经发生过一次
# 「main.rs 提交了 mod knowledge;，但四个新文件还是 untracked」，
# 干净 checkout 直接编不过，而本地全绿。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

TREE=/tmp/ui-headcheck
TARGET=/tmp/ui-headcheck-target
rm -rf "$TREE" "$TARGET"
git worktree prune >/dev/null 2>&1 || true
git worktree add -f --detach "$TREE" HEAD >/dev/null 2>&1 || { echo "FAIL worktree 建不起来"; exit 1; }

( cd "$TREE" && CARGO_TARGET_DIR="$TARGET" cargo build --release --offline 2>&1 | tail -2 )
BIN="$TARGET/release/ui"
if [ ! -x "$BIN" ]; then
  echo "FAIL 干净检出的 HEAD 编译失败"
  git worktree remove --force "$TREE" >/dev/null 2>&1 || true
  exit 1
fi

echo "干净检出的 HEAD 构建通过: $("$BIN" --version)"

( cd "$TREE" && CARGO_TARGET_DIR="$TARGET" cargo test --offline -p ui-core --lib knowledge 2>&1 | tail -3 )

git worktree remove --force "$TREE" >/dev/null 2>&1 || true
rm -rf "$TARGET"
echo "PASS 干净检出可构建、知识单测通过"
