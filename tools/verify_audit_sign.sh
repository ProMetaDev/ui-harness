#!/usr/bin/env bash
# L2 可选：审计导出的 OpenSSH 非对称签名 + append-only 锚定链。
#
# 覆盖：
#   1. 临时 ssh key + allowed_signers，签名通过；
#   2. namespace / identity 绑定；导出被篡改后验签必须失败；
#   3. anchor 连续追加后链校验通过，改中间条必须失败；
#   4. 截断的边界：纯前缀仍自洽，只有给出已知 head/count 才能发现截断；
#   5. 没有 ssh-keygen 时明确 SKIP。
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
    echo "PASS audit sign/anchor: $PASS checks"
    exit 0
  fi
  echo "FAIL audit sign/anchor: $PASS passed / $FAIL failed"
  exit 1
}

# 期望某个退出码；输出留到最后一行，失败时打印尾巴。
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

write_export() {
  # $1=path $2=session $3=text $4=exportedAt
  local out="$1" session="$2" text="$3" ts="$4"
  cat >"$out" <<EOF
{"version":1,"session":"$session","exportedAt":$ts,"total":2,"count":2,"since":0,"limit":0,"events":[{"seq":1,"ts":$ts,"kind":"note","text":"$text","prev":"0000000000000000000000000000000000000000000000000000000000000000","hash":"1111111111111111111111111111111111111111111111111111111111111111"},{"seq":2,"ts":$((ts + 1)),"kind":"note","text":"second","prev":"1111111111111111111111111111111111111111111111111111111111111111","hash":"2222222222222222222222222222222222222222222222222222222222222222"}],"sha256":"3333333333333333333333333333333333333333333333333333333333333333","hmacAlg":"HMAC-SHA256","keyId":"test-key","hmac":null}
EOF
}

python3 -c 'import sys' >/dev/null 2>&1 || skip "python3 不可用（签名/锚定 adapter 是 L2 可选依赖）"
PY="$(command -v python3)"
SIGN="$UI_ROOT/adapters/audit/sign_export.py"
ANCHOR="$UI_ROOT/adapters/audit/anchor.py"
NAMESPACE="ui-audit"
IDENTITY="auditor@example.test"

if [ ! -f "$SIGN" ] || [ ! -f "$ANCHOR" ]; then
  echo "FAIL: adapters/audit/{sign_export,anchor}.py 缺失"
  exit 1
fi

# 先探测 OpenSSH 能力；不支持时整个门禁 SKIP（L2 可选）。
"$PY" "$SIGN" check >/dev/null 2>&1
CHECK_CODE=$?
case "$CHECK_CODE" in
  0) ;;
  2) skip "没有支持 -Y sign/verify 的 ssh-keygen（L2 可选依赖缺失）" ;;
  *)
    echo "FAIL: sign_export.py check 失败（exit $CHECK_CODE）"
    "$PY" "$SIGN" check
    exit 1
    ;;
esac

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ui-audit-sign.XXXXXX") || {
  echo "FAIL: mktemp 失败"
  exit 1
}
trap 'rm -rf "$TMP"' EXIT

echo "=== 1. 非对称签名 / 验签 ==="
KEY="$TMP/audit_ed25519"
if ssh-keygen -q -t ed25519 -N '' -C 'ui-audit test key' -f "$KEY" >/dev/null 2>&1 \
  && [ -s "$KEY" ]; then
  ok "生成临时 ssh key（ed25519，无口令）"
else
  bad "生成临时 ssh key"
  finish
fi
PUB="$(ssh-keygen -y -f "$KEY" 2>/dev/null)"
if [ -n "$PUB" ]; then
  printf '%s %s\n' "$IDENTITY" "$PUB" >"$TMP/allowed_signers"
  ok "生成 allowed_signers"
else
  bad "导出公钥到 allowed_signers"
  finish
fi

EXPORT="$TMP/export.json"
write_export "$EXPORT" default alpha 1790000000000
BEFORE="$(sha256sum "$EXPORT" | cut -d' ' -f1)"

SIG_OUT="$("$PY" "$SIGN" sign --export "$EXPORT" --key "$KEY" --namespace "$NAMESPACE" 2>&1)"
SIG_CODE=$?
if [ "$SIG_CODE" -eq 0 ] && [ -s "$EXPORT.sig" ]; then
  ok "签名成功并生成 detached .sig"
else
  bad "签名" "exit=$SIG_CODE output=$SIG_OUT"
fi
AFTER="$(sha256sum "$EXPORT" | cut -d' ' -f1)"
if [ "$BEFORE" = "$AFTER" ]; then
  ok "签名前后导出 JSON 逐字节不变"
else
  bad "签名修改了导出 JSON"
fi
case "$SIG_OUT" in
  *"PRIVATE KEY"*) bad "签名输出包含私钥内容" ;;
  *) ok "签名输出不含私钥内容" ;;
esac

expect_code 0 "正确 allowed_signers/identity 验签通过" \
  "$PY" "$SIGN" verify --export "$EXPORT" --sig "$EXPORT.sig" \
  --allowed-signers "$TMP/allowed_signers" --identity "$IDENTITY" --namespace "$NAMESPACE"

expect_code 1 "identity 不匹配时验签失败" \
  "$PY" "$SIGN" verify --export "$EXPORT" --sig "$EXPORT.sig" \
  --allowed-signers "$TMP/allowed_signers" --identity "intruder@example.test" \
  --namespace "$NAMESPACE"

expect_code 1 "namespace 不匹配时验签失败" \
  "$PY" "$SIGN" verify --export "$EXPORT" --sig "$EXPORT.sig" \
  --allowed-signers "$TMP/allowed_signers" --identity "$IDENTITY" --namespace "other"

CUSTOM_SIG="$TMP/custom.sig"
expect_code 0 "自定义 --sig 路径签名通过" \
  "$PY" "$SIGN" sign --export "$EXPORT" --key "$KEY" --sig "$CUSTOM_SIG"
expect_code 0 "自定义 .sig 路径验签通过" \
  "$PY" "$SIGN" verify --export "$EXPORT" --sig "$CUSTOM_SIG" \
  --allowed-signers "$TMP/allowed_signers" --identity "$IDENTITY" --namespace "$NAMESPACE"

expect_code 1 "--sig 指向导出文件本身时拒绝覆盖" \
  "$PY" "$SIGN" sign --export "$EXPORT" --key "$KEY" --sig "$EXPORT" --force
GUARD_AFTER="$(sha256sum "$EXPORT" | cut -d' ' -f1)"
if [ "$BEFORE" = "$GUARD_AFTER" ]; then
  ok "拒绝覆盖后导出 JSON 仍未被修改"
else
  bad "拒绝覆盖后导出 JSON 被修改"
fi

cp "$EXPORT" "$TMP/tampered.json"
"$PY" - "$TMP/tampered.json" <<'PY'
import json
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as f:
    data = json.load(f)
data["events"][0]["text"] = "tampered"
with open(path, "w", encoding="utf-8") as f:
    json.dump(data, f, separators=(",", ":"))
PY
expect_code 1 "导出被篡改后验签必须失败" \
  "$PY" "$SIGN" verify --export "$TMP/tampered.json" --sig "$EXPORT.sig" \
  --allowed-signers "$TMP/allowed_signers" --identity "$IDENTITY" --namespace "$NAMESPACE"

# 无 ssh-keygen：sign 必须明确 SKIP（exit 2），不能假装签过。
mkdir -p "$TMP/nobin"
NOKEY_OUT="$(PATH="$TMP/nobin" "$PY" "$SIGN" sign --export "$EXPORT" --key "$KEY" 2>&1)"
NOKEY_CODE=$?
if [ "$NOKEY_CODE" -eq 2 ]; then
  ok "无 ssh-keygen 时 sign 返回 2（SKIP）"
else
  bad "无 ssh-keygen 时 sign 未返回 2" "exit=$NOKEY_CODE output=$NOKEY_OUT"
fi
case "$NOKEY_OUT" in
  *"SKIP:"*) ok "无 ssh-keygen 时输出明确的 SKIP 文案" ;;
  *) bad "无 ssh-keygen 时没有 SKIP 提示" "$NOKEY_OUT" ;;
esac

echo "=== 2. append-only 锚定链 ==="
ANCHORS="$TMP/anchors.jsonl"
EXPORT2="$TMP/export2.json"
EXPORT3="$TMP/export3.json"
write_export "$EXPORT2" default bravo 1790000001000
write_export "$EXPORT3" default charlie 1790000002000

expect_code 0 "anchor append #1" \
  "$PY" "$ANCHOR" append --anchors "$ANCHORS" --export "$EXPORT"
expect_code 0 "anchor append #2" \
  "$PY" "$ANCHOR" append --anchors "$ANCHORS" --export "$EXPORT2"
expect_code 0 "append 2 条后 verify 通过" \
  "$PY" "$ANCHOR" verify --anchors "$ANCHORS" --expect-count 2

if "$PY" - "$ANCHORS" <<'PY'
import os
import stat
import sys

mode = stat.S_IMODE(os.stat(sys.argv[1]).st_mode)
sys.exit(0 if mode == 0o444 else 1)
PY
then
  ok "anchors.jsonl 权限为 0444（近似只读）"
else
  bad "anchors.jsonl 权限不是 0444"
fi

HEAD2="$("$PY" "$ANCHOR" head --anchors "$ANCHORS" --print head)"
if [ "${#HEAD2}" -eq 64 ]; then
  ok "head 输出 64 位哈希"
else
  bad "head 输出异常" "len=${#HEAD2} value=$HEAD2"
fi
expect_code 0 "带已知 head 的 verify 通过（允许继续追加）" \
  "$PY" "$ANCHOR" verify --anchors "$ANCHORS" --expect-head "$HEAD2"

expect_code 0 "anchor append #3" \
  "$PY" "$ANCHOR" append --anchors "$ANCHORS" --export "$EXPORT3"
HEAD3="$("$PY" "$ANCHOR" head --anchors "$ANCHORS" --print head)"
LAST_SHA="$("$PY" "$ANCHOR" head --anchors "$ANCHORS" --print sha256)"
EXPORT3_SHA="$(sha256sum "$EXPORT3" | cut -d' ' -f1)"
if [ "$LAST_SHA" = "$EXPORT3_SHA" ]; then
  ok "锚点 sha256 == 最后一份导出文件的 SHA-256"
else
  bad "锚点 sha256 与导出文件不符" "anchor=$LAST_SHA file=$EXPORT3_SHA"
fi

# 改中间条（第 2 条的 ts）：seq/sha256/prev/ts 都在记录哈希里，链必须断。
MID="$TMP/anchors-middle.jsonl"
cp "$ANCHORS" "$MID"
chmod u+w "$MID"
"$PY" - "$MID" <<'PY'
import json
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as f:
    lines = f.read().splitlines()
entry = json.loads(lines[1])
entry["ts"] = entry["ts"] + 1
lines[1] = json.dumps(entry, separators=(",", ":"))
with open(path, "w", encoding="utf-8") as f:
    f.write("\n".join(lines) + "\n")
PY
expect_code 2 "改中间条目后 verify 失败" \
  "$PY" "$ANCHOR" verify --anchors "$MID"

# 截断：纯前缀本身是一条自洽的链——这正是必须外部记录 head 的原因。
TRUNC="$TMP/anchors-truncated.jsonl"
cp "$ANCHORS" "$TRUNC"
chmod u+w "$TRUNC"
"$PY" - "$TRUNC" <<'PY'
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as f:
    lines = f.read().splitlines()
with open(path, "w", encoding="utf-8") as f:
    f.write("\n".join(lines[:-1]) + "\n")
PY
expect_code 0 "没有已知 head 时，截断后的前缀链仍自洽（边界）" \
  "$PY" "$ANCHOR" verify --anchors "$TRUNC"
expect_code 3 "给出最后 head 后，verify 检出截断" \
  "$PY" "$ANCHOR" verify --anchors "$TRUNC" --expect-head "$HEAD3"
expect_code 3 "给出 count 后，verify 检出截断" \
  "$PY" "$ANCHOR" verify --anchors "$TRUNC" --expect-count 3

finish
