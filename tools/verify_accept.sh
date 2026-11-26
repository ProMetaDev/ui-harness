#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# --accept：验收命令通过才算完成；失败必须非零退出。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
run() {
  local name="$1" accept="$2" expect="$3"
  local ws=/tmp/ui-accept-$name
  rm -rf "$ws"; mkdir -p "$ws"
  # 预置 out.txt：验收命令（test -f out.txt）检查的是工作区里已存在的文件
  printf "hello\n" > "$ws/out.txt"
  cat > "$ws/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"{\"parallel\":[{\"prompt\":\"写文件\"}],\"summary\":\"汇总\"}"},
  {"content":"子任务完成","toolCalls":[{"name":"fs_write","args":{"path":"out.txt","content":"hello"}}]},
  {"content":"完成"}]}
EOF
  "$BIN" team --task "写个 out.txt" --agents 2 --mock "$ws/sc.json" --workspace "$ws" \
    --yes --accept "$accept" > "$ws/log" 2>&1
  local code=$?
  if [ "$code" = "$expect" ]; then
    echo "PASS $name → 退出码 $code"
  else
    echo "FAIL $name → 期望 $expect 实际 $code"
    grep -E '验收' "$ws/log" | head -3
  fi
}

# 验收通过
run ok 'test -f out.txt' 0
# 验收失败（文件不存在）
run bad 'test -f 不存在的文件' 3
