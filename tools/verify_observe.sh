#!/usr/bin/env bash
# 观测式效果：shell 实际写的文件必须出现在日志里；fs_write 不重复记；UI_OBSERVE=0 可关
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-verify-observe
rm -rf "$WS"; mkdir -p "$WS"
cat > "$WS/sc.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"shell 写","toolCalls":[{"name":"shell","args":{"command":"echo hi > observed.txt"}}]},
  {"content":"精确写","toolCalls":[{"name":"fs_write","args":{"path":"declared.txt","content":"x"}}]},
  {"content":"完成"}]}
JSON

"$UI_BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes >/dev/null 2>&1 || {
  echo "FAIL 运行失败"; exit 1; }

python3 - "$WS/.ui/journal.jsonl" <<'PY'
import json, sys
obs = []
for line in open(sys.argv[1], encoding='utf-8'):
    e = json.loads(line)
    if e.get('kind') == 'effect/observed':
        obs.append(e)
if len(obs) != 1:
    print(f"FAIL 期望 1 条 effect/observed，实际 {len(obs)}")
    sys.exit(1)
e = obs[0]
if e.get('tool') != 'shell':
    print(f"FAIL 观测来源应为 shell，实际 {e.get('tool')}")
    sys.exit(1)
added = (e.get('changes') or {}).get('added') or []
if 'observed.txt' not in added:
    print(f"FAIL 没记下 shell 写的文件: {added}")
    sys.exit(1)
if 'declared.txt' in added:
    print("FAIL fs_write 的写入不该出现在观测里（它已精确声明）")
    sys.exit(1)
print(f"PASS 观测到 shell 实际改动: +{len(added)}")
PY
code=$?

# 开关：关掉之后不该再有观测事件
WS2=/tmp/ui-verify-observe-off
rm -rf "$WS2"; mkdir -p "$WS2"
cp "$WS/sc.json" "$WS2/sc.json"
UI_OBSERVE=0 "$UI_BIN" run --task t --mock "$WS2/sc.json" --workspace "$WS2" --yes >/dev/null 2>&1
if grep -q 'effect/observed' "$WS2/.ui/journal.jsonl" 2>/dev/null; then
  echo "FAIL UI_OBSERVE=0 时仍有观测记录"
  exit 1
fi
echo "PASS UI_OBSERVE=0 可关闭观测"
exit $code
