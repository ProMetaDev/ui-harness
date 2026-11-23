#!/usr/bin/env bash
# L2 adapter：把一份 journal 导入 Postgres。
# 需要 psql（显式企业依赖），不属于 ui-core。
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

: "${DATABASE_URL:?需要 DATABASE_URL}"
JOURNAL=${1:?用法: import.sh JOURNAL [SESSION] [WORKSPACE]}
SESSION=${2:-default}
WORKSPACE=${3:-default}

psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$HERE/schema.sql"
python3 "$HERE/export_journal.py" \
  --journal "$JOURNAL" --session "$SESSION" --workspace "$WORKSPACE" \
  | psql "$DATABASE_URL" -v ON_ERROR_STOP=1
echo "PASS imported $JOURNAL session=$SESSION workspace=$WORKSPACE"
