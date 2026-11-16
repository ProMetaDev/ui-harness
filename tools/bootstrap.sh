#!/usr/bin/env bash
# 一条命令说清楚"要跑起来还缺什么、怎么装"，并尽量用国内镜像。
# 不静默改系统：能不用 root 的就不用（mingw 用 apt-get download 解压到 ~/mingw64）。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"

echo "== 当前状态 =="
bash "$UI_ROOT/tools/preflight.sh"
echo
echo "== 缺什么就装什么（命令已按镜像写好）=="

need() { command -v "$1" >/dev/null 2>&1; }

need rustc || cat <<'TIP'
# Rust 工具链（用 rsproxy 镜像）
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y --profile minimal
TIP

need python3 || cat <<'TIP'
# Python 3（日志校验器与验证包需要）
sudo apt-get install -y python3
TIP

need curl || cat <<'TIP'
# curl（HTTP 冒烟测试需要）
sudo apt-get install -y curl
TIP

MINGW="$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix"
if ! need x86_64-w64-mingw32-gcc && [ ! -x "$MINGW" ]; then
  cat <<'TIP'
# Windows 交叉编译用的 mingw（不需要 root：下 .deb 再解压到 ~/mingw64）
# 然后：bash tools/build_windows.sh
mkdir -p ~/mingw64/deb ~/mingw64/root && cd ~/mingw64/deb
for p in gcc-mingw-w64-x86-64 gcc-mingw-w64-x86-64-posix gcc-mingw-w64-base \
         binutils-mingw-w64-x86-64 mingw-w64-x86-64-dev; do apt-get download "$p"; done
for d in ./*.deb; do dpkg-deb -x "$d" ~/mingw64/root; done
TIP
fi

if ! need node; then
  cat <<'TIP'
# Node（可选：只有真模型适配器 tools/deepseek.mjs / chat_compat.mjs 需要）
# nvm 装的话注意它不在非登录 shell 的 PATH 里，适配器要用绝对路径。
TIP
fi

echo
echo "== 装完复查 =="
echo "  bash tools/preflight.sh"
echo "  bash tools/test.sh"
