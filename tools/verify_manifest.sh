#!/usr/bin/env bash
# 门禁：隔离副本的 manifest 必须是 xxHash64 格式，且覆盖工作区里的文件。
#
# 为什么要专门测：manifest 的哈希算法从 SHA-256 换成 xxHash64 是为了性能，
# 而这个格式没有任何别的检查会碰到——改回去了也"看起来正常"。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-verify-manifest
rm -rf "$WS"; mkdir -p "$WS"
head -c 4096 /dev/urandom > "$WS/data.bin"

cat > "$WS/plan.json" <<'JSON'
{"model":"scripted","steps":[
  {"content":"{\"parallel\":[{\"prompt\":\"做点事\"}],\"summary\":\"汇总\"}"},
  {"content":"完成"},
  {"content":"汇总完成"}]}
JSON

"$UI_BIN" team --task t --mock "$WS/plan.json" --workspace "$WS" --isolate --yes >/dev/null 2>&1

manifest=$(find "$WS/.ui/agents" -name manifest.txt 2>/dev/null | head -1)
if [ -z "$manifest" ]; then
  echo "FAIL 没有生成 manifest.txt"
  exit 1
fi

total=$(grep -c . "$manifest" || true)
xxh=$(grep -c '	xxh:[0-9a-f]\{16\}$' "$manifest" || true)
echo "manifest: $manifest（$total 条，xxh 格式 $xxh 条）"

[ "$total" -gt 0 ] || { echo "FAIL manifest 是空的"; exit 1; }
[ "$xxh" -eq "$total" ] || { echo "FAIL 有 $((total - xxh)) 条不是 xxh 格式"; exit 1; }
grep -q '^data.bin	xxh:' "$manifest" || { echo "FAIL manifest 里没有 data.bin"; exit 1; }

echo "PASS manifest 全部为 xxh 格式，且覆盖了工作区文件"
