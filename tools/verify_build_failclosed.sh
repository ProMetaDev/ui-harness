#!/usr/bin/env bash
# 验证 build_windows.sh 是 fail-closed：在**副本**里注入一个编译错误，脚本必须非零退出，
# 且不能打印上一次的旧产物。
set -uo pipefail
SRC=/mnt/x/dsh_ws/harness-v2

# 没有 mingw 链接器就跳过（CI 的 linux job 不装它；windows job 自己会跑构建）
if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 \
   && [ ! -x "$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix" ]; then
  echo "SKIP 没有 mingw 链接器，跳过（这条只在装了交叉工具链的机器上有意义）"
  exit 0
fi
COPY=/tmp/ui-failtest
rm -rf "$COPY" /tmp/ui-failtest-build
mkdir -p "$COPY"
# 只拷构建需要的东西（跳过 .git/out/target）
rsync -a --exclude .git --exclude out --exclude target --exclude .ui "$SRC"/ "$COPY"/

# 注入编译错误
echo 'compile_error!("intentional failure to test fail-closed");' >> "$COPY/ui-cli/src/main.rs"

before=$(sha256sum /home/fairy/ui-target/x86_64-pc-windows-gnu/release/ui.exe 2>/dev/null | cut -c1-16 || echo none)
echo "构建前产物 sha: $before"

# 关键：用独立的构建目录。这个脚本会故意注入编译错误，
# 如果沿用默认的 ~/ui，会把坏源码同步进去，污染后面的所有构建（真实踩过）。
out=$(UI_BUILD_DIR=/tmp/ui-failtest-build bash "$COPY/tools/build_windows.sh" 2>&1)
code=$?
echo "退出码: $code"
echo "$out" | grep -E 'error|FAIL|产物:' | tail -4

fail=0
[ "$code" -ne 0 ] || { echo "FAIL 构建失败却退出 0"; fail=1; }
echo "$out" | grep -q '^产物:' && { echo "FAIL 失败时还打印了产物"; fail=1; }
after=$(sha256sum /home/fairy/ui-target/x86_64-pc-windows-gnu/release/ui.exe 2>/dev/null | cut -c1-16 || echo none)
# 失败后旧产物必须**消失**（否则残留的 exe 会被当成本次结果）
[ "$after" = "none" ] || { echo "FAIL 失败后仍留着旧产物（$after），会被误当成这次的产物"; fail=1; }

rm -rf "$COPY" /tmp/ui-failtest-build
[ "$fail" -eq 0 ] && echo "PASS fail-closed：非零退出、不打印产物、旧产物已删除不留残留"
exit "$fail"
