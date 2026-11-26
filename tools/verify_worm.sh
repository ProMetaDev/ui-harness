#!/usr/bin/env bash
# L2 可选：本地 WORM 风格内容寻址对象库（审计导出存储）。
#
# 覆盖：
#   1. put 后 get 逐字节一致，对象/sidecar 都是 0444，verify 通过；
#   2. 重复 put 相同内容不覆盖、不追加 index；换保留期被拒绝；
#   3. 保留期内 delete 被拒绝（退出 2），对象仍在；
#   4. 到期（--now）delete 成功，对象与 sidecar 消失，tombstone 落盘；
#   5. 同一 id 不能“复活”（删除后重新 put 被拒绝）；
#   6. 篡改对象内容 / sidecar / index（改字段、截断、撕裂）后 verify 失败；
#   7. 没有 python3 时整个门禁 SKIP（退出 0）；
#   8. --now 是测试专用时钟：没有 UI_WORM_TEST_CLOCK=1 时 exit 2。
set -uo pipefail
command -v python3 >/dev/null 2>&1 || {
  echo "SKIP: python3 不可用（WORM store 是 L2 可选依赖）"
  exit 0
}
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

PASS=0
FAIL=0
ok() {
  PASS=$((PASS + 1))
  printf '  ok   %s\n' "$1"
}
bad() {
  FAIL=$((FAIL + 1))
  printf '  FAIL %s\n' "$1"
  [ -n "${2:-}" ] && printf '       %s\n' "$2"
}
finish() {
  echo
  if [ "$FAIL" -eq 0 ]; then
    echo "PASS worm store: $PASS checks"
    exit 0
  fi
  echo "FAIL worm store: $PASS passed / $FAIL failed"
  exit 1
}

expect_code() {
  local want="$1" desc="$2"
  shift 2
  local log="$TMP/last.log" code
  "$@" >"$log" 2>&1
  code=$?
  if [ "$code" -eq "$want" ]; then
    ok "$desc (exit $code)"
  else
    bad "$desc" "want exit $want, got $code: $(tail -n 2 "$log" | tr '\n' ' ')"
  fi
}

PY="$(command -v python3)"
STORE_PY="$UI_ROOT/adapters/worm/store.py"
SELF="$UI_ROOT/tools/verify_worm.sh"
[ -f "$STORE_PY" ] || {
  echo "FAIL: adapters/worm/store.py 缺失"
  exit 1
}
worm() { "$PY" "$STORE_PY" "$@"; }

id_from() {
  local id="${1#*id=}"
  printf '%s' "${id%% *}"
}

line_count() {
  wc -l <"$1" | tr -d '[:space:]'
}

# 对象 id == 文件字节的 sha256。
file_sha() {
  "$PY" -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ui-worm.XXXXXX") || {
  echo "FAIL: mktemp 失败"
  exit 1
}
trap 'rm -rf "$TMP"' EXIT

NOW=$(date +%s)
RET=$((NOW + 2))

cat >"$TMP/export-a.json" <<'EOF'
{"version":1,"session":"default","exportedAt":1790000000000,"total":1,"count":1,"since":0,"limit":0,"events":[{"seq":1,"ts":1790000000000,"kind":"note","text":"alpha"}],"sha256":"1111111111111111111111111111111111111111111111111111111111111111","hmacAlg":"HMAC-SHA256","keyId":"test-key","hmac":null}
EOF
cat >"$TMP/export-b.json" <<'EOF'
{"version":1,"session":"default","exportedAt":1790000001000,"total":1,"count":1,"since":0,"limit":0,"events":[{"seq":1,"ts":1790000001000,"kind":"note","text":"bravo"}],"sha256":"2222222222222222222222222222222222222222222222222222222222222222","hmacAlg":"HMAC-SHA256","keyId":"test-key","hmac":null}
EOF

echo "=== 0. 测试时钟必须显式开启 ==="
# `--now` 是测试专用时钟：没有 UI_WORM_TEST_CLOCK=1 就必须 exit 2。
# 这一条守着"生产 CLI 不能随手改保留期判定"的边界。
env -u UI_WORM_TEST_CLOCK "$PY" "$STORE_PY" put --store "$TMP/no-clock" \
  --file "$TMP/export-a.json" --retention-days 1 --now "$NOW" \
  >"$TMP/no-clock.log" 2>&1
CLOCK_CODE=$?
if [ "$CLOCK_CODE" -eq 2 ] && grep -q 'UI_WORM_TEST_CLOCK' "$TMP/no-clock.log"; then
  ok "未设 UI_WORM_TEST_CLOCK 时 --now 被拒（exit 2，提示测试专属）"
else
  bad "未设 UI_WORM_TEST_CLOCK 时 --now 没有被拒" \
    "exit=$CLOCK_CODE log=$(tail -n 2 "$TMP/no-clock.log" | tr '\n' ' ')"
fi

# 后面的用例要操纵保留期时钟，显式打开测试时钟。
export UI_WORM_TEST_CLOCK=1

echo "=== 1. put / get / verify ==="
STORE="$TMP/store-a"
OUT="$(worm put --store "$STORE" --file "$TMP/export-a.json" \
  --retention-until "$RET" --now "$NOW" 2>&1)"
CODE=$?
ID_A="$(id_from "$OUT")"
if [ "$CODE" -eq 0 ] && [ "${#ID_A}" -eq 64 ]; then
  ok "put 返回 64 位 sha256 对象 id"
else
  bad "put 失败" "exit=$CODE output=$OUT"
  finish
fi
EXPECT_ID="$(file_sha "$TMP/export-a.json")"
if [ "$ID_A" = "$EXPECT_ID" ]; then
  ok "对象 id == 输入字节的 sha256（内容寻址）"
else
  bad "对象 id 与输入哈希不符" "id=$ID_A sha256=$EXPECT_ID"
fi
if [ -f "$STORE/objects/$ID_A" ]; then
  ok "对象落在 objects/<sha256>"
else
  bad "objects/<sha256> 不存在"
fi
if "$PY" - "$STORE/objects/$ID_A" "$STORE/index.jsonl" <<'PY'
import os
import stat
import sys

for path in sys.argv[1:]:
    mode = stat.S_IMODE(os.stat(path).st_mode)
    if mode != 0o444:
        print(f"{path}: mode {mode:04o} != 0444")
        sys.exit(1)
PY
then
  ok "对象与 index 权限为 0444"
else
  bad "对象或 index 权限不是 0444"
fi
if "$PY" - "$STORE/objects/$ID_A.meta.json" "$ID_A" "$RET" <<'PY'
import json
import sys

path, oid, retention = sys.argv[1], sys.argv[2], int(sys.argv[3])
meta = json.load(open(path, encoding="utf-8"))
if set(meta) != {"sha256", "size", "retentionUntil", "createdAt"}:
    print(f"sidecar fields: {sorted(meta)}")
    sys.exit(1)
if meta["sha256"] != oid or meta["retentionUntil"] != retention or meta["size"] <= 0:
    print(f"sidecar mismatch: {meta}")
    sys.exit(1)
PY
then
  ok "sidecar 含 retentionUntil/createdAt/size/sha256 且与请求一致"
else
  bad "sidecar 内容不对"
fi
expect_code 0 "put 后 verify 通过" worm verify --store "$STORE"
expect_code 0 "get --out 读回对象" worm get --store "$STORE" "$ID_A" --out "$TMP/got-a.json"
if cmp -s "$TMP/got-a.json" "$TMP/export-a.json"; then
  ok "get 读回的字节与 put 前逐字节一致"
else
  bad "get 读回内容与输入不一致"
fi
worm get --store "$STORE" "$ID_A" >"$TMP/got-stdout.json" 2>"$TMP/get.err"
if cmp -s "$TMP/got-stdout.json" "$TMP/export-a.json"; then
  ok "get 直接写 stdout 也逐字节一致"
else
  bad "get stdout 内容与输入不一致"
fi

echo "=== 2. 重复 put 不覆盖 / 保留期不可变 ==="
LINES_BEFORE="$(line_count "$STORE/index.jsonl")"
OUT="$(worm put --store "$STORE" --file "$TMP/export-a.json" \
  --retention-until "$RET" --now "$NOW" 2>&1)"
CODE=$?
LINES_AFTER="$(line_count "$STORE/index.jsonl")"
if [ "$CODE" -eq 0 ] && [ "$LINES_BEFORE" = "$LINES_AFTER" ]; then
  ok "重复 put 相同内容成功且不追加 index（幂等，不覆盖）"
else
  bad "重复 put 改变了 index" "exit=$CODE lines $LINES_BEFORE -> $LINES_AFTER"
fi
case "$OUT" in
  *new=no*) ok "重复 put 明确报告 new=no" ;;
  *) bad "重复 put 没有报告 new=no" "$OUT" ;;
esac
expect_code 2 "同一内容换保留期被拒绝（retention 不可变）" \
  worm put --store "$STORE" --file "$TMP/export-a.json" \
  --retention-until "$((RET + 60))" --now "$NOW"
if [ "$(line_count "$STORE/index.jsonl")" = "$LINES_BEFORE" ]; then
  ok "被拒绝的 put 没有留下 index 记录"
else
  bad "被拒绝的 put 改动了 index"
fi
expect_code 0 "拒绝之后 verify 仍通过" worm verify --store "$STORE"

echo "=== 3. 保留期内拒绝删除 ==="
expect_code 2 "retention 未到期时 delete 被拒绝" \
  worm delete --store "$STORE" "$ID_A" --now "$NOW"
if [ -f "$STORE/objects/$ID_A" ] && [ ! -e "$STORE/tombstones/$ID_A.json" ]; then
  ok "拒绝删除后对象仍在、没有 tombstone"
else
  bad "拒绝删除后对象状态不对"
fi
if [ "$(line_count "$STORE/index.jsonl")" = "$LINES_BEFORE" ]; then
  ok "被拒绝的 delete 没有写 index"
else
  bad "被拒绝的 delete 写了 index"
fi

echo "=== 4. 到期删除 + tombstone ==="
expect_code 0 "retention 到期后 delete 成功（--now == retentionUntil）" \
  worm delete --store "$STORE" "$ID_A" --now "$RET"
if [ ! -e "$STORE/objects/$ID_A" ]; then
  ok "删除后对象文件消失"
else
  bad "删除后对象文件仍存在"
fi
if [ ! -e "$STORE/objects/$ID_A.meta.json" ]; then
  ok "删除后 sidecar 消失"
else
  bad "删除后 sidecar 仍存在"
fi
if [ -f "$STORE/tombstones/$ID_A.json" ]; then
  ok "删除后写下了 tombstone"
else
  bad "tombstone 缺失"
fi
expect_code 1 "删除后 get 返回 not found（退出 1）" worm get --store "$STORE" "$ID_A"
if "$PY" - "$STORE/index.jsonl" <<'PY'
import json
import sys

lines = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
ops = [r["op"] for r in lines]
if ops != ["put", "delete", "tombstone"]:
    print(f"ops={ops}")
    sys.exit(1)
if lines[0]["prev"] != "0" * 64:
    print("genesis prev is not 64 zeros")
    sys.exit(1)
for prev, cur in zip(lines, lines[1:]):
    if cur["prev"] != prev["hash"]:
        print(f"prev chain broken at seq {cur['seq']}")
        sys.exit(1)
PY
then
  ok "index 依次记录 put/delete/tombstone，prev 链完整"
else
  bad "index 记录或 prev 链不对"
fi
expect_code 0 "删除 tombstone 后 verify 通过" worm verify --store "$STORE"
expect_code 0 "重复 delete 幂等（不再改 index）" \
  worm delete --store "$STORE" "$ID_A" --now "$((RET + 1))"
if [ "$(line_count "$STORE/index.jsonl")" = "3" ]; then
  ok "重复 delete 没有追加记录"
else
  bad "重复 delete 追加了记录" "lines=$(line_count "$STORE/index.jsonl")"
fi
if worm list --store "$STORE" --all --json | grep -q '"deleted": true'; then
  ok "list --all 把该对象标记为 deleted"
else
  bad "list --all 没有标记 deleted"
fi
if [ "$(worm list --store "$STORE" --json)" = "[]" ]; then
  ok "list 默认只列 live 对象（已删除的不列）"
else
  bad "list 默认列表包含了已删除对象"
fi
expect_code 2 "已删除的 id 不能被重新 put（不允许复活）" \
  worm put --store "$STORE" --file "$TMP/export-a.json" \
  --retention-until "$((NOW + 3600))" --now "$NOW"

echo "=== 5. 篡改对象 / sidecar 后 verify 失败 ==="
STORE_B="$TMP/store-b"
OUT="$(worm put --store "$STORE_B" --file "$TMP/export-b.json" \
  --retention-until "$((NOW + 3600))" --now "$NOW" 2>&1)"
ID_B="$(id_from "$OUT")"
if [ "${#ID_B}" -eq 64 ]; then
  ok "第二个 store put 成功"
else
  bad "第二个 store put 失败" "$OUT"
  finish
fi
"$PY" - "$STORE_B/objects/$ID_B" <<'PY'
import os
import sys

path = sys.argv[1]
os.chmod(path, 0o600)
with open(path, "ab") as f:
    f.write(b"tampered")
PY
expect_code 2 "对象内容被追加后 verify 失败" worm verify --store "$STORE_B"
expect_code 2 "对象被篡改后 get 拒绝返回（哈希不符）" \
  worm get --store "$STORE_B" "$ID_B" --out "$TMP/tampered.out"

STORE_C="$TMP/store-c"
OUT="$(worm put --store "$STORE_C" --file "$TMP/export-b.json" \
  --retention-until "$((NOW + 3600))" --now "$NOW" 2>&1)"
ID_C="$(id_from "$OUT")"
"$PY" - "$STORE_C/objects/$ID_C.meta.json" <<'PY'
import json
import os
import sys

path = sys.argv[1]
os.chmod(path, 0o600)
meta = json.load(open(path, encoding="utf-8"))
meta["retentionUntil"] += 1
open(path, "w", encoding="utf-8").write(json.dumps(meta, separators=(",", ":")) + "\n")
PY
expect_code 2 "sidecar 的 retentionUntil 被改后 verify 失败" worm verify --store "$STORE_C"

echo "=== 6. 改 / 截断 index 后 verify 失败 ==="
MUT="$TMP/store-mut"
cp -r "$STORE" "$MUT"
"$PY" - "$MUT/index.jsonl" <<'PY'
import json
import os
import sys

path = sys.argv[1]
os.chmod(path, 0o600)
lines = open(path, encoding="utf-8").read().splitlines()
record = json.loads(lines[0])
record["retentionUntil"] = 1
lines[0] = json.dumps(record, separators=(",", ":"))
open(path, "w", encoding="utf-8").write("\n".join(lines) + "\n")
PY
expect_code 2 "index 记录被改（哈希对不上）后 verify 失败" worm verify --store "$MUT"

TRUNC="$TMP/store-trunc"
cp -r "$STORE" "$TRUNC"
"$PY" - "$TRUNC/index.jsonl" <<'PY'
import os
import sys

path = sys.argv[1]
os.chmod(path, 0o600)
lines = open(path, encoding="utf-8").read().splitlines()
open(path, "w", encoding="utf-8").write("\n".join(lines[:-1]) + "\n")
PY
expect_code 2 "index 最后一条（tombstone）被截掉后 verify 失败" worm verify --store "$TRUNC"

TORN="$TMP/store-torn"
cp -r "$STORE" "$TORN"
"$PY" - "$TORN/index.jsonl" <<'PY'
import os
import sys

path = sys.argv[1]
os.chmod(path, 0o600)
data = open(path, "rb").read()
open(path, "wb").write(data[:-10])
PY
expect_code 2 "index 尾部被截断成不完整 JSON 后 verify 失败" worm verify --store "$TORN"

echo "=== 7. 无 python3 时 SKIP ==="
mkdir -p "$TMP/nobin"
SKIP_CODE=0
PATH="$TMP/nobin" "${BASH:-bash}" "$SELF" >"$TMP/skip.log" 2>&1 || SKIP_CODE=$?
if [ "$SKIP_CODE" -eq 0 ] && grep -q '^SKIP' "$TMP/skip.log"; then
  ok "无 python3 时门禁 SKIP（退出 0）"
else
  bad "无 python3 时没有 SKIP" "exit=$SKIP_CODE log=$(tail -n 2 "$TMP/skip.log" | tr '\n' ' ')"
fi

finish
