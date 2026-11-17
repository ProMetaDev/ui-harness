#!/usr/bin/env bash
# 交叉编译 Windows 版 ui.exe（x86_64-pc-windows-gnu）。
#
# 链接器按顺序找：
#   1. PATH 里的 x86_64-w64-mingw32-gcc（CI 上 apt 装的）
#   2. ~/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix（本地解压的，不需要 root）
# 都找不到就明确报错并给出安装命令，而不是让 cargo 抛一堆链接错误。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"

export PATH="$HOME/.cargo/bin:$PATH"

find_linker() {
  local c
  for c in x86_64-w64-mingw32-gcc x86_64-w64-mingw32-gcc-posix; do
    if command -v "$c" >/dev/null 2>&1; then
      command -v "$c"
      return 0
    fi
  done
  for c in \
    "$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix" \
    "$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc"; do
    if [ -x "$c" ]; then
      echo "$c"
      return 0
    fi
  done
  return 1
}

LINKER=$(find_linker) || {
  cat >&2 <<'TIP'
找不到 mingw 链接器。任选一种：
  # CI / 有 root：
  sudo apt-get install -y gcc-mingw-w64-x86-64
  # 无 root：下 .deb 解压到 ~/mingw64
  bash tools/bootstrap.sh   # 会打印完整命令
TIP
  exit 1
}

echo "== 链接器 =="
echo "$LINKER"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$LINKER"

if command -v rustup >/dev/null 2>&1; then
  if ! rustup target list --installed 2>/dev/null | grep -q 'x86_64-pc-windows-gnu'; then
    echo "== 安装 target x86_64-pc-windows-gnu =="
    rustup target add x86_64-pc-windows-gnu || exit 1
  fi
fi

# 必须在构建副本里跑：直接在 X: 上跑 cargo 会写 Cargo.lock，
# 而 dev.sh 同时在 rsync X: → 副本，可能拷到写了一半的锁文件（真实踩过两次）。
bash "$UI_ROOT/tools/sync.sh" >/dev/null
cd "$UI_BUILD_DIR" || exit 1
echo "== 构建 ui.exe =="
EXE="$UI_TARGET_DIR/x86_64-pc-windows-gnu/release/ui.exe"
# 先删旧产物：否则构建失败时会拿上一次的 exe 当结果，脚本还会退出 0（真实踩过）
rm -f "$EXE"

set -o pipefail
cargo build --offline --release --target x86_64-pc-windows-gnu -p ui-cli 2>&1 | tail -20
STATUS=${PIPESTATUS[0]}
if [ "$STATUS" -ne 0 ]; then
  echo "FAIL 交叉编译失败（cargo 退出码 $STATUS）" >&2
  exit "$STATUS"
fi

if [ ! -f "$EXE" ]; then
  echo "没有产物：$EXE（构建算是"成功"却没产出，说明状态不对）" >&2
  exit 1
fi
echo
ls -lh "$EXE" | awk '{print "产物: " $9 " (" $5 ")"}'
sha256sum "$EXE" | awk '{print "sha256: " $1}'
