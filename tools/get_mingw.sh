#!/usr/bin/env bash
# 不装系统包：把 mingw 链接器下载到本地解压（不需要 root）
set -uo pipefail
DEST="$HOME/mingw64"
mkdir -p "$DEST/deb" "$DEST/root"
cd "$DEST/deb" || exit 1

PKGS="gcc-mingw-w64-x86-64 gcc-mingw-w64-x86-64-posix gcc-mingw-w64-base binutils-mingw-w64-x86-64 mingw-w64-x86-64-dev"

echo "== 下载（apt-get download 不需要 root）=="
ok=0
for p in $PKGS; do
  if apt-get download "$p" >/dev/null 2>&1; then
    echo "  ✓ $p"
    ok=$((ok+1))
  else
    echo "  ✗ $p（可能不在源里，继续）"
  fi
done
echo "下载成功 $ok 个"

echo
echo "== 解压到 $DEST/root =="
for deb in ./*.deb; do
  dpkg-deb -x "$deb" "$DEST/root" 2>/dev/null || echo "  解压失败: $deb"
done

echo
echo "== 找链接器 =="
find "$DEST/root" -name 'x86_64-w64-mingw32-gcc*' -type f 2>/dev/null | head -5
find "$DEST/root" -name 'usr' -maxdepth 2 -type d 2>/dev/null | head -3
