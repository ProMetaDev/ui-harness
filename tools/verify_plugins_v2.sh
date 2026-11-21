#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 插件：能力声明必须显式；调用走 plugin_call；声明的效果要进日志。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
WS=/tmp/ui-plug
rm -rf "$WS"; mkdir -p "$WS/.ui"
cp fixtures/plugin/demo.py "$WS/.ui/demo.py"

cat > "$WS/.ui/plugins.json" <<EOF
{"plugins":[
  {"name":"demo","cmd":"python3 $WS/.ui/demo.py","tools":[
    {"name":"echo","summary":"原样返回","effects":[]},
    {"name":"write","summary":"写文件","effects":["fs.write out.txt"]}
  ]}
]}
EOF

fail() { echo "FAIL: $1"; exit 1; }

echo "=== ui plugins ==="
"$BIN" plugins --workspace "$WS" | tee "$WS/list.txt"
grep -q 'demo' "$WS/list.txt" || fail "列表里没有 demo"
grep -q 'fs.write' "$WS/list.txt" || fail "没显示声明的效果"
echo "PASS 列表与效果声明"

echo
echo "=== 调用插件（无副作用）==="
cat > "$WS/sc.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"调插件","toolCalls":[{"name":"plugin_call","args":{"plugin":"demo","tool":"echo","args":{"text":"你好"}}}]},
  {"content":"完"}]}
EOF
out=$("$BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --plugins "$WS/.ui/plugins.json" --yes 2>&1)
echo "$out" | grep -q 'echo: 你好' || fail "插件结果没回来：$out"
echo "PASS 插件调用"

echo
echo "=== 无副作用的插件：仍隐式带 proc.exec ==="
python3 - "$WS/.ui/journal.jsonl" <<'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8"):
    ev = json.loads(line)
    if ev.get("kind") == "tool/request" and ev.get("tool") == "plugin_call":
        eff = ev.get("effects") or []
        print("effects:", json.dumps(eff, ensure_ascii=False))
        ok = any(e.get("kind") == "proc.exec" and e.get("target") == "plugin:demo:echo" for e in eff)
        print("PASS 空声明仍有 proc.exec" if ok else "FAIL 效果不对")
        assert ok
        break
PY

echo
echo "=== 有副作用的插件：效果必须进日志 ==="
cat > "$WS/sc2.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"写文件","toolCalls":[{"name":"plugin_call","args":{"plugin":"demo","tool":"write","args":{"path":"out.txt","content":"hi"}}}]},
  {"content":"完"}]}
EOF
rm -f "$WS/.ui/journal.jsonl"
out=$("$BIN" run --task t --mock "$WS/sc2.json" --workspace "$WS" --plugins "$WS/.ui/plugins.json" --yes 2>&1)
echo "$out" | grep -q 'wrote out.txt' || fail "写操作没成功：$out"
python3 - "$WS/.ui/journal.jsonl" <<'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8"):
    ev = json.loads(line)
    if ev.get("kind") == "tool/request" and ev.get("tool") == "plugin_call":
        eff = ev.get("effects") or []
        print("effects:", json.dumps(eff, ensure_ascii=False))
        ok = any(e.get("kind") == "fs.write" for e in eff)
        print("PASS 声明的 fs.write 进了日志" if ok else "FAIL 效果没进日志")
        break
PY

echo
echo "=== 没有声明 effects 的插件必须加载失败 ==="
printf '{"plugins":[{"name":"bad","cmd":"true","tools":[{"name":"x"}]}]}\n' > "$WS/.ui/bad.json"
out=$("$BIN" plugins --workspace "$WS" --plugins "$WS/.ui/bad.json" 2>&1 || true)
echo "$out" | grep -q '没有声明 effects' || fail "没拒绝缺声明的插件：$out"
echo "PASS 拒绝缺声明"

echo
echo "=== 未知效果类型必须加载失败 ==="
printf '{"plugins":[{"name":"bad2","cmd":"true","tools":[{"name":"x","effects":["fs.rede **"]}]}]}\n' > "$WS/.ui/bad2.json"
out=$("$BIN" plugins --workspace "$WS" --plugins "$WS/.ui/bad2.json" 2>&1 || true)
echo "$out" | grep -q '未知效果' || fail "没拒绝未知效果：$out"
echo "PASS 拒绝未知效果"
