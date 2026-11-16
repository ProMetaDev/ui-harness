#!/usr/bin/env bash
# 多 provider：注册表能列、能选、选错要报清楚；不同 provider 走不同适配器。
set -uo pipefail
cd /mnt/x/dsh_ws/harness-v2
BIN="$HOME/ui/target/release/ui"
WS=/tmp/ui-prov
rm -rf "$WS"; mkdir -p "$WS/.ui"

# 两个假适配器，证明"换 provider 真的换了进程"
cat > "$WS/.ui/alpha.sh" <<'EOF'
#!/bin/sh
cat >/dev/null
printf '{"content":"alpha 回复 model=%s","toolCalls":[],"usageTokens":7}\n' "$UI_MODEL"
EOF
cat > "$WS/.ui/beta.sh" <<'EOF'
#!/bin/sh
cat >/dev/null
printf '{"content":"beta 回复 model=%s","toolCalls":[],"usageTokens":9}\n' "$UI_MODEL"
EOF
chmod +x "$WS/.ui/alpha.sh" "$WS/.ui/beta.sh"

cat > "$WS/.ui/providers.json" <<EOF
{
  "default": "alpha",
  "providers": {
    "alpha": { "cmd": "sh $WS/.ui/alpha.sh", "model": "alpha-1" },
    "beta":  { "cmd": "sh $WS/.ui/beta.sh",  "model": "beta-2" }
  }
}
EOF

fail() { echo "FAIL: $1"; exit 1; }

echo "=== ui providers ==="
"$BIN" providers --workspace "$WS" | tee "$WS/prov.txt"
grep -q 'alpha' "$WS/prov.txt" || fail "列表里没有 alpha"
grep -q 'beta' "$WS/prov.txt" || fail "列表里没有 beta"
echo "PASS 列表"

echo
echo "=== 默认 provider（alpha）==="
out=$("$BIN" run --task t --workspace "$WS" --yes 2>&1)
echo "$out" | grep -q 'alpha 回复 model=alpha-1' || fail "默认没用 alpha：$out"
echo "PASS 默认走 alpha"

echo
echo "=== --provider beta ==="
out=$("$BIN" run --task t --workspace "$WS" --yes --provider beta 2>&1)
echo "$out" | grep -q 'beta 回复 model=beta-2' || fail "切换失败：$out"
echo "PASS 切换到 beta"

echo
echo "=== 不存在的 provider ==="
out=$("$BIN" run --task t --workspace "$WS" --yes --provider nope 2>&1 || true)
echo "$out" | grep -q '没有名为' || fail "错误信息不清楚：$out"
echo "PASS 错误信息列出了可用的"
