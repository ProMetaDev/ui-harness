#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 多 seed 对照表：三个包 → 一张表；输入不一致时必须拒绝求均值。
set -uo pipefail
cd "$UI_ROOT"
BIN="$UI_BIN"
BASE=/tmp/ui-table
rm -rf "$BASE"; mkdir -p "$BASE"

mk() {
  local name="$1" acc="$2" datahash="$3" seed="$4"
  local d="$BASE/$name"
  mkdir -p "$d/artifacts"
  cat > "$d/meta.json" <<EOF
{"name":"$name","createdMs":1,"workspace":"/tmp/x","journalEvents":10,
 "env":{"os":"Ubuntu 22.04.5 LTS","kernel":"6.18.0","cpus":32,"python":"3.10.12"},
 "commands":[]}
EOF
  printf '{"path":"data.csv","sha256":"%s","bytes":12,"readBy":{"seq":3,"tool":"fs_read"}}\n' "$datahash" > "$d/inputs.jsonl"
  printf '{"path":"metrics.json","sha256":"h","bytes":20,"producedBy":{"seq":9,"tool":"fs_write"}}\n' > "$d/manifest.jsonl"
  printf '{"seed":%s,"accuracy":%s}\n' "$seed" "$acc" > "$d/artifacts/metrics.json"
}

H1=$(printf 'same' | sha256sum | cut -d' ' -f1)
H2=$(printf 'changed' | sha256sum | cut -d' ' -f1)
mk s1 0.80 "$H1" 1
mk s2 0.82 "$H1" 2
mk s3 0.81 "$H1" 3

echo "=== 三个包，同一份数据 ==="
"$BIN" lab table "$BASE/s1" "$BASE/s2" "$BASE/s3"; echo "退出码: $?"

echo
echo "=== 把 s3 的数据换成另一个版本 ==="
printf '{"path":"data.csv","sha256":"%s","bytes":12,"readBy":{"seq":3,"tool":"fs_read"}}\n' "$H2" > "$BASE/s3/inputs.jsonl"
set +e
"$BIN" lab table "$BASE/s1" "$BASE/s2" "$BASE/s3" | tail -4
echo "退出码: ${PIPESTATUS[0]}"
set -e

echo
echo "=== JSON 形态 ==="
"$BIN" lab table "$BASE/s1" "$BASE/s2" --json
