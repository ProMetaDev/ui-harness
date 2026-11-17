#!/usr/bin/env bash
# OS 沙箱（Landlock）：区外写入被拒、区内可写、关闭开关生效
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

# 内核不支持就明确 SKIP（不假装通过，也不假装失败）
abi=$(python3 - <<'PY'
import ctypes, ctypes.util
libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
rc = libc.syscall(444, None, ctypes.c_size_t(0), ctypes.c_uint32(1))
print(rc if rc > 0 else 0)
PY
)
if [ "${abi:-0}" -lt 1 ]; then
  echo "SKIP 内核不支持 Landlock（需要 Linux 5.13+ 且启用）——沙箱在这种机器上会 fail closed"
  exit 0
fi
echo "Landlock ABI: $abi"

WS=/tmp/ui-verify-sandbox
rm -rf "$WS"; mkdir -p "$WS"
OUTSIDE="$HOME/.ui-sandbox-probe.txt"
rm -f "$OUTSIDE"

cat > "$WS/sc.json" <<JSON
{"model":"scripted","steps":[
  {"content":"区内写","toolCalls":[{"name":"shell","args":{"command":"echo ok > inside.txt"}}]},
  {"content":"区外写","toolCalls":[{"name":"shell","args":{"command":"echo evil > $OUTSIDE"}}]},
  {"content":"完成"}]}
JSON

out=$("$UI_BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1)
# 有些环境「有 Landlock syscall 但 apply 失败」（容器/受限内核）。
# 这时我们的实现会 fail closed，shell 全部失败——这是环境限制，不是回归。
if printf '%s' "$out" | grep -q '沙箱不可用'; then
  echo "SKIP 这个环境里 Landlock 不可用（shell 已 fail closed）。CI 上可能出现，本地 WSL 不会。"
  exit 0
fi

[ -f "$WS/inside.txt" ] || { echo "FAIL 工作区内写不了（且不是"沙箱不可用"）"; echo "$out" | tail -3; exit 1; }
if [ -f "$OUTSIDE" ]; then
  rm -f "$OUTSIDE"
  echo "FAIL 工作区外竟然写成功了（沙箱没生效）"
  exit 1
fi
echo "PASS 工作区内可写 · 工作区外被拒"

# 关闭开关：UI_SANDBOX=0 时区外写入应当成功（证明开关真的在起作用）
WS2=/tmp/ui-verify-sandbox-off
rm -rf "$WS2"; mkdir -p "$WS2"
cp "$WS/sc.json" "$WS2/sc.json"
UI_SANDBOX=0 "$UI_BIN" run --task t --mock "$WS2/sc.json" --workspace "$WS2" --yes >/dev/null 2>&1
if [ -f "$OUTSIDE" ]; then
  rm -f "$OUTSIDE"
  echo "PASS UI_SANDBOX=0 时限制解除（开关有效）"
else
  echo "FAIL 关了沙箱还写不出去，说明开关没接上"
  exit 1
fi
exit 0
