#!/usr/bin/env bash
# 模拟 GitHub Actions：全新 clone 到 /tmp，不依赖 ~/ui 副本，跑 CI 快路径。
set -uo pipefail
rm -rf /tmp/ci-sim
mkdir -p /tmp/ci-sim
cd /tmp/ci-sim || exit 1

git clone -q /mnt/x/dsh_ws/harness-v2 repo || { echo "clone 失败"; exit 1; }
cd repo || exit 1
echo "clone 完成：$(git log --oneline -1)"
echo "工作区大小: $(du -sh . | cut -f1)"
echo

export UI_BUILD_DIR="$PWD"
export UI_TARGET_DIR="$PWD/target"
export UI_GIT_DIR="$PWD"

start=$(date +%s)
bash tools/ci.sh --fast 2>&1 | tail -25
code=${PIPESTATUS[0]}
echo
echo "CI 退出码: $code · 用时 $(( $(date +%s) - start ))s"
