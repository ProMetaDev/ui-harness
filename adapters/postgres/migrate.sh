#!/usr/bin/env bash
# L2 adapter: idempotent schema migration entrypoint.
#
# This is intentionally separate from import.sh: import.sh imports a journal
# (a COPY stream, which is not idempotent), while this script only brings the
# schema to the latest version and can be run as often as needed.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

: "${DATABASE_URL:?需要 DATABASE_URL}"

psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$HERE/schema.sql"
echo "PASS migrations applied: $HERE/schema.sql"
