#!/usr/bin/env bash
# 在 CI 里准备 mingw 链接器：**只下载 .deb 解包**，不 install。
#
# 为什么不用 `apt-get install gcc-mingw-w64-x86-64`：
#   1. 元包会把 posix + win32 + g++ 全拉下来，体积是 posix 单包的数倍；
#   2. 装进 /usr 之后没法进 actions/cache，于是**每次** CI 都要重下（实测 28 分钟）；
#   3. `tools/build_windows.sh` 本来就支持 `~/mingw64/root/usr/bin/...` 这种解包路径。
#
# 解包到 ~/mingw64 之后整个目录可以缓存，缓存命中时这一步只需 1–2 秒。
set -euo pipefail

ROOT="${HOME}/mingw64/root"
WORK=/tmp/mingw-debs

if [ -x "$ROOT/usr/bin/x86_64-w64-mingw32-gcc-posix" ]; then
  echo "已有解包好的工具链：$ROOT"
  exit 0
fi

rm -rf "$WORK"
mkdir -p "$WORK" "$ROOT"
cd "$WORK"

PKGS=(
  gcc-mingw-w64-x86-64-posix
  gcc-mingw-w64-base
  binutils-mingw-w64-x86-64
  mingw-w64-x86-64-dev
)

echo "== 下载 .deb（只要 posix 变体）=="
# 先直接下：机器上已经有包索引时不需要 update（update 才是慢的那一步）。
if ! apt-get download "${PKGS[@]}" 2>/dev/null; then
  echo "包索引过期或缺失，更新一次再试"
  sudo apt-get update -qq
  apt-get download "${PKGS[@]}"
fi

echo "== 解包到 $ROOT =="
for d in ./*.deb; do
  dpkg-deb -x "$d" "$ROOT"
done

if [ ! -x "$ROOT/usr/bin/x86_64-w64-mingw32-gcc-posix" ]; then
  echo "解包后找不到 x86_64-w64-mingw32-gcc-posix" >&2
  ls -l "$ROOT/usr/bin" 2>/dev/null | head -20 >&2
  exit 1
fi

echo "== 自检：链接器能不能跑 =="
"$ROOT/usr/bin/x86_64-w64-mingw32-gcc-posix" --version | head -1
echo "OK: $ROOT"
