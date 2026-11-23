#!/usr/bin/env bash
# L2 adapter 的离线测试：不连 Postgres，只验证 COPY 导出契约。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"
if ! command -v python3 >/dev/null 2>&1; then
  echo "SKIP: python3 不可用（Postgres adapter 是 L2 可选依赖）"
  exit 0
fi
python3 -m unittest adapters/postgres/test_export.py
echo "PASS postgres adapter export tests"
