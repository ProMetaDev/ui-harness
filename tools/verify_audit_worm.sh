#!/usr/bin/env bash
# L2 可选：`/api/audit/export` -> OpenSSH 签名 -> append-only 锚定链 -> WORM 对象库。
#
# 覆盖：
#   1. 临时 SSH key + allowed_signers + fixture export，完整管线跑通；
#   2. 产物逐字节一致，anchor 的 sha256 == 导出哈希 == store 对象 id；
#   3. 篡改导出 / 签名 / 对象 / index 后 verify 必须失败；
#   4. 保留期内 delete 被拒绝（退出 2），对象仍在；
#   5. 外部 anchor head 是截断边界：纯前缀自洽，给出 head 后必须失败；
#   6. URL 模式校验 HTTP 200 + JSON，token 不出现在日志；404 / 非 JSON 失败且无半成品；
#   7. 没有 python3 或 ssh-keygen 时整体 SKIP（退出 0）。
command -v python3 >/dev/null 2>&1 ||
  { echo "SKIP: python3 不可用（审计管线是 L2 可选依赖）"; exit 0; }
command -v ssh-keygen >/dev/null 2>&1 ||
  { echo "SKIP: ssh-keygen 不可用（OpenSSH >= 8.1 是签名依赖）"; exit 0; }

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -uo pipefail
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
skip() {
  echo "SKIP: $1"
  exit 0
}
finish() {
  echo
  if [ "$FAIL" -eq 0 ]; then
    echo "PASS audit->worm pipeline: $PASS checks"
    exit 0
  fi
  echo "FAIL audit->worm pipeline: $PASS passed / $FAIL failed"
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

file_sha() {
  "$PY" -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}

write_export() {
  # $1=path $2=text $3=exportedAt
  cat >"$1" <<EOF
{"version":1,"session":"default","exportedAt":$3,"total":2,"count":2,"since":0,"limit":0,"events":[{"seq":1,"ts":$3,"kind":"note","text":"$2"},{"seq":2,"ts":$(( $3 + 1 )),"kind":"note","text":"second"}],"sha256":"3333333333333333333333333333333333333333333333333333333333333333","hmacAlg":"HMAC-SHA256","keyId":"test-key","hmac":null}
EOF
}

PY="$(command -v python3)"

PIPE="$UI_ROOT/tools/audit_export_to_worm.py"
SIGN="$UI_ROOT/adapters/audit/sign_export.py"
ANCHOR="$UI_ROOT/adapters/audit/anchor.py"
STORE_PY="$UI_ROOT/adapters/worm/store.py"
SELF="$UI_ROOT/tools/verify_audit_worm.sh"
for f in "$PIPE" "$SIGN" "$ANCHOR" "$STORE_PY"; do
  [ -f "$f" ] || {
    echo "FAIL: 缺少 $f"
    exit 1
  }
done

# 先探测 OpenSSH -Y sign/verify；不支持时整个门禁 SKIP（L2 可选）。
"$PY" "$SIGN" check >/dev/null 2>&1
CHECK_CODE=$?
case "$CHECK_CODE" in
  0) ;;
  2) skip "没有支持 -Y sign/verify 的 ssh-keygen（OpenSSH >= 8.1 是 L2 依赖）" ;;
  *)
    echo "FAIL: sign_export.py check 失败（exit $CHECK_CODE）"
    exit 1
    ;;
esac

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ui-audit-worm.XXXXXX") || {
  echo "FAIL: mktemp 失败"
  exit 1
}
SRV_PID=""
cleanup() {
  if [ -n "$SRV_PID" ]; then
    kill "$SRV_PID" 2>/dev/null || true
    wait "$SRV_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

IDENTITY="auditor@example.test"
NAMESPACE="ui-audit"
KEY="$TMP/audit_ed25519"
ALLOWED="$TMP/allowed_signers"
if ssh-keygen -q -t ed25519 -N '' -C 'ui-audit worm test key' -f "$KEY" >/dev/null 2>&1 \
  && [ -s "$KEY" ]; then
  ok "生成临时 ed25519 key"
else
  bad "生成临时 ed25519 key"
  finish
fi
PUB="$(ssh-keygen -y -f "$KEY" 2>/dev/null)"
if [ -n "$PUB" ]; then
  printf '%s %s\n' "$IDENTITY" "$PUB" >"$ALLOWED"
  ok "生成 allowed_signers"
else
  bad "导出公钥到 allowed_signers"
  finish
fi

EXPORT="$TMP/export.json"
write_export "$EXPORT" alpha 1790000000000
SHA1="$(file_sha "$EXPORT")"

echo "=== 1. 完整管线：export -> 签名 -> anchor -> WORM ==="
OUT="$TMP/out"
NAME="export"
ANCHORS="$TMP/anchors.jsonl"
STORE="$TMP/store"
"$PY" "$PIPE" run \
  --export "$EXPORT" \
  --out-dir "$OUT" \
  --name "$NAME" \
  --key "$KEY" \
  --allowed-signers "$ALLOWED" \
  --identity "$IDENTITY" \
  --namespace "$NAMESPACE" \
  --anchors "$ANCHORS" \
  --store "$STORE" \
  --retention-days 1 >"$TMP/run.log" 2>&1
RUN_CODE=$?
if [ "$RUN_CODE" -eq 0 ]; then
  ok "管线退出 0"
else
  bad "管线失败" "exit=$RUN_CODE $(tail -n 3 "$TMP/run.log" | tr '\n' ' ')"
  finish
fi
grep -q '^PASS audit->worm pipeline:' "$TMP/run.log" &&
  ok "输出明确的 PASS 行" ||
  bad "缺少 PASS 行"

for f in "$OUT/$NAME.json" "$OUT/$NAME.json.sig"; do
  [ -s "$f" ] && ok "产物存在且非空: ${f#"$TMP/"}" || bad "产物缺失或为空: $f"
done
if cmp -s "$OUT/$NAME.json" "$EXPORT"; then
  ok "导出的字节逐字节保存（未重新格式化）"
else
  bad "导出的字节与输入不一致"
fi
if grep -q 'PRIVATE KEY' "$TMP/run.log"; then
  bad "管线日志包含私钥标记"
else
  ok "管线日志不含私钥标记"
fi

HEAD1="$("$PY" "$ANCHOR" head --anchors "$ANCHORS" --print head)"
COUNT1="$("$PY" "$ANCHOR" head --anchors "$ANCHORS" --print count)"
SHA_IN_ANCHOR="$("$PY" "$ANCHOR" head --anchors "$ANCHORS" --print sha256)"
if [ "$COUNT1" = "1" ] && [ "$SHA_IN_ANCHOR" = "$SHA1" ]; then
  ok "anchors.jsonl 有 1 条记录且 sha256 == 导出哈希"
else
  bad "anchor 记录不对" "count=$COUNT1 anchor_sha=$SHA_IN_ANCHOR export_sha=$SHA1"
fi
if [ "${#HEAD1}" -eq 64 ]; then
  ok "anchor head 是 64 位哈希"
else
  bad "anchor head 长度异常" "$HEAD1"
fi
if [ -f "$STORE/objects/$SHA1" ] && cmp -s "$STORE/objects/$SHA1" "$EXPORT"; then
  ok "WORM 对象 id == 导出 sha256，内容逐字节一致"
else
  bad "WORM 对象缺失或内容不一致"
fi
expect_code 0 "WORM store verify 通过" "$PY" "$STORE_PY" verify --store "$STORE"
expect_code 0 "管线 verify 子命令通过" \
  "$PY" "$PIPE" verify --export "$OUT/$NAME.json" --sig "$OUT/$NAME.json.sig" \
  --anchors "$ANCHORS" --store "$STORE" \
  --allowed-signers "$ALLOWED" --identity "$IDENTITY" --namespace "$NAMESPACE"

echo "=== 2. 篡改导出 / 签名 / 对象 / index 后必须失败 ==="
TAMPER="$TMP/tamper-export"
cp -r "$OUT" "$TAMPER"
printf '\n' >>"$TAMPER/$NAME.json"
expect_code 1 "导出被追加字节后 verify 失败" \
  "$PY" "$PIPE" verify --export "$TAMPER/$NAME.json" --sig "$TAMPER/$NAME.json.sig" \
  --anchors "$ANCHORS" --store "$STORE" \
  --allowed-signers "$ALLOWED" --identity "$IDENTITY" --namespace "$NAMESPACE"

TAMPER_SIG="$TMP/tamper-sig"
cp -r "$OUT" "$TAMPER_SIG"
if "$PY" - "$TAMPER_SIG/$NAME.json.sig" <<'PY'
import sys

path = sys.argv[1]
lines = open(path, encoding="ascii").read().splitlines()
for i, line in enumerate(lines):
    if line and not line.startswith("-----"):
        chars = list(line)
        chars[10] = "A" if chars[10] != "A" else "B"
        lines[i] = "".join(chars)
        break
else:
    raise SystemExit("no armored signature line")
with open(path, "w", encoding="ascii", newline="\n") as f:
    f.write("\n".join(lines) + "\n")
PY
then
  expect_code 1 "签名被改后 verify 失败" \
    "$PY" "$PIPE" verify --export "$TAMPER_SIG/$NAME.json" \
    --sig "$TAMPER_SIG/$NAME.json.sig" --anchors "$ANCHORS" --store "$STORE" \
    --allowed-signers "$ALLOWED" --identity "$IDENTITY" --namespace "$NAMESPACE"
else
  bad "构造被篡改的签名失败"
fi

TAMPER_OBJ="$TMP/tamper-object"
cp -r "$STORE" "$TAMPER_OBJ"
if "$PY" - "$TAMPER_OBJ/objects/$SHA1" <<'PY'
import os
import sys

path = sys.argv[1]
os.chmod(path, 0o600)
with open(path, "ab") as f:
    f.write(b"tampered")
PY
then
  expect_code 1 "对象内容被改后 verify 失败" \
    "$PY" "$PIPE" verify --export "$OUT/$NAME.json" --sig "$OUT/$NAME.json.sig" \
    --anchors "$ANCHORS" --store "$TAMPER_OBJ" \
    --allowed-signers "$ALLOWED" --identity "$IDENTITY" --namespace "$NAMESPACE"
else
  bad "构造被篡改的对象失败"
fi

TAMPER_INDEX="$TMP/tamper-index"
cp -r "$STORE" "$TAMPER_INDEX"
if "$PY" - "$TAMPER_INDEX/index.jsonl" <<'PY'
import json
import os
import sys

path = sys.argv[1]
os.chmod(path, 0o600)
lines = open(path, encoding="utf-8").read().splitlines()
record = json.loads(lines[0])
record["retentionUntil"] += 1
lines[0] = json.dumps(record, separators=(",", ":"))
with open(path, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(lines) + "\n")
PY
then
  expect_code 1 "index 记录被改后 verify 失败" \
    "$PY" "$PIPE" verify --export "$OUT/$NAME.json" --sig "$OUT/$NAME.json.sig" \
    --anchors "$ANCHORS" --store "$TAMPER_INDEX" \
    --allowed-signers "$ALLOWED" --identity "$IDENTITY" --namespace "$NAMESPACE"
else
  bad "构造被篡改的 index 失败"
fi

echo "=== 3. 外部 anchor head 是截断边界 ==="
EXPORT2="$TMP/export2.json"
write_export "$EXPORT2" bravo 1790000001000
OUT2="$TMP/out2"
"$PY" "$PIPE" run \
  --export "$EXPORT2" \
  --out-dir "$OUT2" \
  --name export2 \
  --key "$KEY" \
  --allowed-signers "$ALLOWED" \
  --identity "$IDENTITY" \
  --namespace "$NAMESPACE" \
  --anchors "$ANCHORS" \
  --store "$STORE" \
  --retention-days 1 >"$TMP/run2.log" 2>&1
if [ $? -eq 0 ]; then
  ok "第二份导出追加到同一条 anchor 链与同一个 store"
else
  bad "第二份导出管线失败" "$(tail -n 3 "$TMP/run2.log" | tr '\n' ' ')"
  finish
fi
HEAD2="$("$PY" "$ANCHOR" head --anchors "$ANCHORS" --print head)"
COUNT2="$("$PY" "$ANCHOR" head --anchors "$ANCHORS" --print count)"
if [ "$COUNT2" = "2" ]; then
  ok "anchor 链增长到 2 条"
else
  bad "anchor 链条数不对" "count=$COUNT2"
fi

TRUNC="$TMP/anchors-truncated.jsonl"
cp "$ANCHORS" "$TRUNC"
chmod u+w "$TRUNC"
"$PY" - "$TRUNC" <<'PY'
import sys

path = sys.argv[1]
lines = open(path, encoding="utf-8").read().splitlines()
with open(path, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(lines[:-1]) + "\n")
PY
expect_code 1 "给出已知 head 后，截断的 anchor 链必须失败" \
  "$PY" "$PIPE" verify --export "$OUT/$NAME.json" --sig "$OUT/$NAME.json.sig" \
  --anchors "$TRUNC" --store "$STORE" \
  --allowed-signers "$ALLOWED" --identity "$IDENTITY" --namespace "$NAMESPACE" \
  --expect-head "$HEAD2"
expect_code 0 "没有已知 head 时，截断后的前缀链本身仍自洽（边界）" \
  "$PY" "$PIPE" verify --export "$OUT/$NAME.json" --sig "$OUT/$NAME.json.sig" \
  --anchors "$TRUNC" --store "$STORE" \
  --allowed-signers "$ALLOWED" --identity "$IDENTITY" --namespace "$NAMESPACE"

echo "=== 4. 保留期内 delete 被拒绝 ==="
LINES_BEFORE="$(wc -l <"$STORE/index.jsonl" | tr -d '[:space:]')"
expect_code 2 "retention 未到期时 delete 拒绝（退出 2）" \
  "$PY" "$STORE_PY" delete --store "$STORE" "$SHA1" --now "$(date +%s)"
if [ -f "$STORE/objects/$SHA1" ] && [ ! -e "$STORE/tombstones/$SHA1.json" ]; then
  ok "拒绝删除后对象仍在、没有 tombstone"
else
  bad "拒绝删除后对象状态不对"
fi
LINES_AFTER="$(wc -l <"$STORE/index.jsonl" | tr -d '[:space:]')"
if [ "$LINES_BEFORE" = "$LINES_AFTER" ]; then
  ok "被拒绝的 delete 没有追加 index 记录"
else
  bad "被拒绝的 delete 写了 index" "lines $LINES_BEFORE -> $LINES_AFTER"
fi

echo "=== 5. URL 模式：HTTP 200 / JSON / token 不入日志 ==="
mkdir -p "$TMP/srv"
cp "$EXPORT" "$TMP/srv/export.json"
printf 'not json\n' >"$TMP/srv/bad.json"
PORT="$(ui_free_port 17930)"
"$PY" -m http.server "$PORT" --bind 127.0.0.1 --directory "$TMP/srv" \
  >"$TMP/http.log" 2>&1 &
SRV_PID=$!
URL_READY=0
for _ in $(seq 1 50); do
  if "$PY" - "$PORT" <<'PY'
import sys
import urllib.request

try:
    urllib.request.urlopen(
        "http://127.0.0.1:%s/export.json" % sys.argv[1], timeout=1
    ).read()
except Exception:
    sys.exit(1)
PY
  then
    URL_READY=1
    break
  fi
  sleep 0.1
done
if [ "$URL_READY" -ne 1 ]; then
  echo "  skip URL 集成（本地 http.server 未就绪，环境限制）"
else
  TOKEN="audit-token-$RANDOM$RANDOM"
  URL_OUT="$TMP/url-out"
  UI_AUDIT_TOKEN="$TOKEN" "$PY" "$PIPE" run \
    --url "http://127.0.0.1:$PORT/export.json" \
    --out-dir "$URL_OUT" \
    --name urlexport \
    --key "$KEY" \
    --allowed-signers "$ALLOWED" \
    --identity "$IDENTITY" \
    --namespace "$NAMESPACE" \
    --anchors "$TMP/url-anchors.jsonl" \
    --store "$TMP/url-store" \
    --retention-days 1 >"$TMP/url-run.log" 2>&1
  if [ $? -eq 0 ]; then
    ok "URL 导出（HTTP 200 + JSON）跑通"
  else
    bad "URL 导出失败" "$(tail -n 3 "$TMP/url-run.log" | tr '\n' ' ')"
  fi
  if grep -qF "$TOKEN" "$TMP/url-run.log"; then
    bad "token 出现在管线输出里"
  else
    ok "token 没有出现在管线输出里"
  fi
  if [ -s "$URL_OUT/urlexport.json" ] \
    && cmp -s "$URL_OUT/urlexport.json" "$EXPORT"; then
    ok "URL 响应字节与 fixture 一致"
  else
    bad "URL 导出的产物缺失或内容不一致"
  fi

  expect_code 1 "URL 404 时管线失败" \
    "$PY" "$PIPE" run \
    --url "http://127.0.0.1:$PORT/missing.json" \
    --out-dir "$TMP/url-404-out" --name missing --key "$KEY" \
    --anchors "$TMP/url-404-anchors.jsonl" --store "$TMP/url-404-store" \
    --retention-days 1
  if [ ! -e "$TMP/url-404-out" ]; then
    ok "404 失败后没有留下输出目录"
  else
    bad "404 失败后留下了输出目录"
  fi

  expect_code 1 "URL 返回非 JSON 时管线失败" \
    "$PY" "$PIPE" run \
    --url "http://127.0.0.1:$PORT/bad.json" \
    --out-dir "$TMP/url-bad-out" --name bad --key "$KEY" \
    --anchors "$TMP/url-bad-anchors.jsonl" --store "$TMP/url-bad-store" \
    --retention-days 1
  if [ ! -e "$TMP/url-bad-out" ]; then
    ok "非 JSON 失败后没有留下输出目录"
  else
    bad "非 JSON 失败后留下了输出目录"
  fi

  kill "$SRV_PID" 2>/dev/null || true
  wait "$SRV_PID" 2>/dev/null || true
  SRV_PID=""
fi

echo "=== 6. 缺依赖时整体 SKIP ==="
mkdir -p "$TMP/nobin"
SKIP_CODE=0
PATH="$TMP/nobin" "${BASH:-bash}" "$SELF" >"$TMP/skip-python.log" 2>&1 || SKIP_CODE=$?
if [ "$SKIP_CODE" -eq 0 ] && grep -q '^SKIP' "$TMP/skip-python.log"; then
  ok "无 python3 时整体 SKIP（退出 0）"
else
  bad "无 python3 时没有 SKIP" "exit=$SKIP_CODE $(tail -n 2 "$TMP/skip-python.log" | tr '\n' ' ')"
fi

mkdir -p "$TMP/onlypy"
cat >"$TMP/onlypy/python3" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
chmod +x "$TMP/onlypy/python3"
SKIP_CODE=0
PATH="$TMP/onlypy" "${BASH:-bash}" "$SELF" >"$TMP/skip-ssh.log" 2>&1 || SKIP_CODE=$?
if [ "$SKIP_CODE" -eq 0 ] && grep -q '^SKIP' "$TMP/skip-ssh.log"; then
  ok "无可用 ssh-keygen 时整体 SKIP（退出 0）"
else
  bad "无可用 ssh-keygen 时没有 SKIP" "exit=$SKIP_CODE $(tail -n 2 "$TMP/skip-ssh.log" | tr '\n' ' ')"
fi

finish
