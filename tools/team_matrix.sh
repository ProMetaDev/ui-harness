#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 协作模式矩阵：三种性质不同的任务各跑一次，记录成败、成本、产出。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
ROOT=/tmp/team-m
rm -rf "$ROOT"; mkdir -p "$ROOT"

NODE_BIN=$(command -v node || echo /home/fairy/.nvm/versions/node/v22.23.3/bin/node)
export UI_MODEL_CMD="$NODE_BIN "$UI_ROOT"/tools/deepseek.mjs"

mk_ws() {
  local d="$1"
  mkdir -p "$d"
  cat > "$d/math.js" <<'EOF'
export function add(a, b) { return a + b; }
export function mul(a, b) { return a * b; }
export function div(a, b) {
  if (b === 0) throw new Error("divide by zero");
  return a / b;
}
EOF
  cat > "$d/README.md" <<'EOF'
# math

三个纯函数：add / mul / div。
EOF
}

run_case() {
  local name="$1" agents="$2" task="$3"
  local ws="$ROOT/$name"
  mk_ws "$ws"
  echo "══════ $name（$agents 个代理）══════"
  local start=$(date +%s)
  "$BIN" team --task "$task" --agents "$agents" --workspace "$ws" --yes --max-calls 40 \
    > "$ROOT/$name.log" 2>&1
  local code=$?
  local secs=$(( $(date +%s) - start ))
  local tok=$(grep -oE '[0-9]+ tok' "$ROOT/$name.log" | tail -1 | tr -d ' tok')
  echo "退出码 $code · ${secs}s · ${tok:-?} tok"
  echo "--- 文件 ---"
  ( cd "$ws" && ls -1 | tr '\n' ' '; echo )
  echo "--- 产物大小 ---"
  ( cd "$ws" && find . -maxdepth 2 -type f ! -path './.ui/*' -printf '%s\t%p\n' 2>/dev/null | sort -k2 )
  echo
}

run_case analysis 2 "只做分析，不要改任何文件：读这个 JS 小项目，指出测试覆盖上的三个最大缺口，并说明每个缺口该怎么补（给出建议的用例名即可）。输出一份简短报告。"

run_case code 2 "改代码：让 add 支持可变参数（add(1,2,3) === 6），同时保持 add(1,2) === 3 的旧行为。改完要能说明改了什么、旧行为为什么没变。不要动 mul 和 div。"

run_case docs 2 "写文档：给这个 JS 小项目写一份 CONTRIBUTING.md，说明如何新增一个函数、如何自测、代码风格约定。基于现有代码事实写，不要编造不存在的工具或脚本。"

echo "══════ 汇总 ══════"
python3 - "$ROOT" <<'PY'
import json, os, sys
root = sys.argv[1]
for name in ("analysis", "code", "docs"):
    log = os.path.join(root, name + ".log")
    ws = os.path.join(root, name)
    if not os.path.exists(log):
        print(f"{name}: 没有日志"); continue
    text = open(log, encoding="utf-8", errors="replace").read()
    jp = os.path.join(ws, ".ui", "journal.jsonl")
    events = []
    if os.path.exists(jp):
        for line in open(jp, encoding="utf-8"):
            try: events.append(json.loads(line))
            except Exception: pass
    kinds = {}
    for e in events:
        kinds[e.get("kind","")] = kinds.get(e.get("kind",""), 0) + 1
    files = []
    for base, _, fs in os.walk(ws):
        if "/.ui" in base: continue
        for f in fs:
            files.append(os.path.relpath(os.path.join(base, f), ws))
    fails = sum(1 for e in events if e.get("kind") == "tool/end" and e.get("ok") is False)
    print(f"{name:9s} files={sorted(files)} toolfail={fails} "
          f"delegate={kinds.get('delegate/end',0)} plan={kinds.get('tool/request',0)}")
PY
