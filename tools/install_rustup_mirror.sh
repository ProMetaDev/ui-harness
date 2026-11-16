#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 用国内镜像装 rustup + Windows 交叉目标（只装在 WSL 的 ~/.cargo，不动 Windows）
set -uo pipefail
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
export CARGO_HOME="$HOME/.cargo"

echo "== 现有工具链 =="
which cargo; cargo --version; which rustc

if [ -x "$HOME/.cargo/bin/rustup" ]; then
  echo "已有 rustup，跳过安装"
else
  echo "== 下载 rustup-init（rsproxy 镜像）=="
  curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh -o /tmp/rustup-init.sh || {
    echo "镜像脚本下载失败"; exit 1; }
  sh /tmp/rustup-init.sh -y --default-toolchain stable --profile minimal \
    --no-modify-path 2>&1 | tail -8
fi

export PATH="$HOME/.cargo/bin:$PATH"
echo "== 版本 =="
"$HOME/.cargo/bin/rustup" --version || exit 1
"$HOME/.cargo/bin/rustc" --version || exit 1

echo "== 加 Windows 目标 =="
"$HOME/.cargo/bin/rustup" target add x86_64-pc-windows-gnu 2>&1 | tail -4
"$HOME/.cargo/bin/rustup" target list --installed
