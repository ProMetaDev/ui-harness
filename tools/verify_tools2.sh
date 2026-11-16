#!/usr/bin/env bash
# 工具面：grep / patch / todo / tree / fs_mv 都要真的能用
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
WS=/tmp/ui-tools2
rm -rf "$WS"; mkdir -p "$WS/src"

cat > "$WS/src/app.js" <<'EOF'
export function add(a, b) {
  return a + b;
}

export function mul(a, b) {
  return a * b;
}
EOF

cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"搜索","toolCalls":[{"name":"grep","args":{"pattern":"export function [a-z]+","path":"src","include":"*.js"}}]},
  {"content":"打补丁","toolCalls":[{"name":"patch","args":{"path":"src/app.js","diff":"--- a/src/app.js\n+++ b/src/app.js\n@@ -1,3 +1,4 @@\n export function add(a, b) {\n+  // 补丁插入的注释\n   return a + b;\n }\n"}}]},
  {"content":"建树","toolCalls":[{"name":"tree","args":{"path":".","depth":2}}]},
  {"content":"待办","toolCalls":[{"name":"todo","args":{"action":"add","text":"补测试"}}]},
  {"content":"改名","toolCalls":[{"name":"fs_mv","args":{"from":"src/app.js","to":"src/main.js"}}]},
  {"content":"完"}]}
EOF

echo "=== 跑一轮（5 个新工具）==="
"$BIN" run --task "试新工具" --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1 | grep -E '→' | head -8

echo
echo "=== 结果核对 ==="
grep -q '补丁插入的注释' "$WS/src/main.js" && echo "PASS patch 生效 + fs_mv 改名" || echo "FAIL patch/mv"
[ -f "$WS/.ui/todos.json" ] && echo "PASS todo 落盘" || echo "FAIL todo"
grep -q '补测试' "$WS/.ui/todos.json" && echo "PASS todo 内容对" || echo "FAIL todo 内容"
[ -f "$WS/src/app.js" ] && echo "FAIL 旧文件还在" || echo "PASS 旧文件已移走"

echo
echo "=== patch 上下文不匹配时必须拒绝 ==="
cat > "$WS/bad.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"乱打补丁","toolCalls":[{"name":"patch","args":{"path":"src/main.js","diff":"@@ -1,1 +1,1 @@\n- 这行不存在\n+ 替换\n"}}]},
  {"content":"完"}]}
EOF
"$BIN" run --task t --mock "$WS/bad.json" --workspace "$WS" --yes 2>&1 | grep -E '→' | head -2
