#!/usr/bin/env bash
# OIDC e2e 门禁：准备 ui.exe（没有就构建），再跑 tools/oidc_e2e.py。
#
# 链路：mock IdP /userinfo → adapters/oidc/oidc_proxy.py → ui serve --trusted-proxy
#      → /api/admin（HTTP 断言；装了 Chrome/Edge 时再加一组 CDP 浏览器断言）。
#
# 退出码：0 = PASS 或明确 SKIP（没有 ui.exe 且构建环境不可用）；
#         1 = 任一 HTTP/journal/日志断言失败，或构建失败。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

UI_EXE="${UI_EXE:-$UI_ROOT/out/ui.exe}"
export UI_EXE

native_build() {
  command -v cargo >/dev/null 2>&1 || return 1
  if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 \
    && [ ! -x "$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix" ] \
    && [ ! -x "$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc" ]; then
    return 1
  fi
  echo "== 没有 $UI_EXE，先构建 ui.exe =="
  bash "$UI_ROOT/tools/build_windows.sh" || return 1
  mkdir -p "$UI_ROOT/out"
  cp "$UI_TARGET_DIR/x86_64-pc-windows-gnu/release/ui.exe" "$UI_EXE" || return 1
}

wsl_build() {
  command -v wsl.exe >/dev/null 2>&1 || return 2
  local win_root wsl_root distro chosen=""
  # 默认发行版不一定装了 mingw；逐个发行版找 cargo + mingw。
  # 必须用进程替换并给 wsl 重定向 stdin：否则 wsl 会把管道里的下一行吃掉。
  while IFS= read -r distro; do
    [ -n "$distro" ] || continue
    if wsl.exe -d "$distro" bash -lc 'command -v cargo >/dev/null 2>&1 && { command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 || [ -x "\$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix" ] || [ -x "\$HOME/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc" ]; }' </dev/null 2>/dev/null; then
      chosen="$distro"
      break
    fi
  done < <(wsl.exe -l -q 2>/dev/null | tr -d '\000\015')

  if [ -z "$chosen" ]; then
    echo "WSL 里没有同时装了 cargo 与 mingw 的发行版" >&2
    return 2
  fi

  # Git Bash 的 wsl.exe 调用会把反斜杠路径吃掉，用 cygpath -m 的斜杠形式。
  win_root="$(cygpath -m "$UI_ROOT" 2>/dev/null || printf '%s' "$UI_ROOT")"
  wsl_root="$(wsl.exe wslpath -a "$win_root" 2>/dev/null | tr -d '\r')"
  [ -n "${wsl_root:-}" ] || return 2

  echo "== 没有 $UI_EXE，尝试用 WSL($chosen) 里的 cargo/mingw 构建 =="
  # 内层变量必须留给 WSL 的 bash 展开：wsl.exe 的命令行还会做一层变量替换，
  # 所以用 \$ 把 `$` 保留到 bash（否则 source env.sh 之前就被替换成空串）。
  wsl.exe -d "$chosen" bash -lc 'cd '"$wsl_root"' && source tools/env.sh && bash tools/build_windows.sh && mkdir -p out && cp "\$UI_TARGET_DIR/x86_64-pc-windows-gnu/release/ui.exe" out/ui.exe' </dev/null
  [ -f "$UI_EXE" ] || return 2
}

if [ ! -f "$UI_EXE" ]; then
  if native_build; then
    :
  elif wsl_build; then
    :
  else
    echo "SKIP: 找不到 $UI_EXE，也没有可用的 cargo/mingw（或 WSL）构建环境。"
    echo "      先 bash tools/build_windows.sh，把产物复制到 out/ui.exe，或用 UI_EXE=... 指定。"
    exit 0
  fi
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "FAIL: 需要 python3" >&2
  exit 1
fi

exec python3 "$UI_ROOT/tools/oidc_e2e.py" "$@"
