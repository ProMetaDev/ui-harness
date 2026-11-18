#!/usr/bin/env bash
# 外部评审 P0-1 复现与回归：secrets/** 的免审批读取路径。
#
# 关键：泄漏发生在**工具输出**里（模型看得见），journal 的 tool/end 只有 ok/meta。
# 所以判据必须看 stdout，而不是只 grep 日志。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-leakcheck
CANARY=sk-LEAK-CANARY-1234
rm -rf "$WS"; mkdir -p "$WS/secrets" "$WS/src"
echo "{\"api_key\":\"$CANARY\"}" > "$WS/secrets/astra.json"
echo 'fn main() {}' > "$WS/src/main.rs"

fail=0

# 内容类：fs_search / grep —— 断言 stdout 里没有 canary
content_case() { # label, toolcall
  local label="$1" call="$2"
  rm -rf "$WS/.ui"
  cat > "$WS/sc.json" <<JSON
{"model":"scripted","steps":[{"content":"x","toolCalls":[$call]},{"content":"done"}]}
JSON
  local out
  out=$("$UI_BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1)
  if printf '%s' "$out" | grep -q "$CANARY"; then
    echo "LEAKED   $label"
    printf '%s' "$out" | grep -m1 "$CANARY" | cut -c1-100
    fail=1
  else
    echo "BLOCKED  $label"
  fi
}

# 目录类：tree / grep(path=secrets) —— 断言要么被拒、要么输出里没有 secrets 条目
listing_case() { # label, toolcall, 禁止出现的片段
  local label="$1" call="$2" needle="${3:-secrets}"
  rm -rf "$WS/.ui"
  cat > "$WS/sc.json" <<JSON
{"model":"scripted","steps":[{"content":"x","toolCalls":[$call]},{"content":"done"}]}
JSON
  local out
  out=$("$UI_BIN" run --task t --mock "$WS/sc.json" --workspace "$WS" --yes 2>&1)
  if printf '%s' "$out" | grep -q -- "$needle"; then
    echo "LEAKED   $label（输出里出现了 $needle）"
    printf '%s' "$out" | grep -m1 -- "$needle" | cut -c1-100
    fail=1
  else
    echo "BLOCKED  $label"
  fi
}

content_case "fs_search(pattern='.')"     '{"name":"fs_search","args":{"pattern":"."}}'
content_case "fs_search(pattern='api')"   '{"name":"fs_search","args":{"pattern":"api"}}'
content_case "grep(path='.')"             '{"name":"grep","args":{"pattern":"sk-LEAK","path":"."}}'
content_case "grep(path='secrets')"       '{"name":"grep","args":{"pattern":"api_key","path":"secrets"}}'
listing_case "tree(path='secrets')"       '{"name":"tree","args":{"path":"secrets"}}' 'astra.json'
listing_case "tree(path='.')"             '{"name":"tree","args":{"path":"."}}'

echo
if [ "$fail" -eq 0 ]; then
  echo "PASS secrets 的所有已知路径都被拦住"
else
  echo "FAIL 仍有路径能读到 secrets（见上）"
fi
exit "$fail"
