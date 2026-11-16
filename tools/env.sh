#!/usr/bin/env bash
# 所有脚本共用的环境探测。被 source，不单独执行。
#
# 设计目标：**换一台机器、换一个目录，脚本仍然能跑**。
# 之前路径写死在 /mnt/x/dsh_ws/harness-v2 和 $HOME/ui，只有本机能用。

# 仓库根目录：以本文件位置为准，不看当前 cwd
UI_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export UI_ROOT

# 构建副本（cargo 在 ext4 上快得多；Windows 挂载盘上编译会很慢）
export UI_BUILD_DIR="${UI_BUILD_DIR:-$HOME/ui}"
# 构建产物目录
export UI_TARGET_DIR="${UI_TARGET_DIR:-$HOME/ui-target}"
# 所有 cargo 调用统一落到这里：否则有的脚本用默认 target，产物会分裂成两份
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
export UI_BIN="$UI_TARGET_DIR/release/ui"
# ????????,????????????(???? .git)
export UI_GIT_DIR="$UI_ROOT"
export UI_WIN_BIN="$UI_BUILD_DIR/target/x86_64-pc-windows-gnu/release/ui.exe"

# 找一个空闲端口：测试脚本不再写死 7802/7799 这类端口，避免残留进程导致偶发红
ui_free_port() {
  local base="${1:-17800}"
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$base" <<'PY'
import socket, sys
base = int(sys.argv[1])
for p in range(base, base + 200):
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", p))
        print(p)
        sys.exit(0)
    except OSError:
        continue
    finally:
        s.close()
print(base + 500)
PY
  else
    echo $((base + RANDOM % 500))
  fi
}

# 等端口可用（最多 6 秒）
ui_wait_port() {
  local port="$1"
  for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$port/api/state" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}
