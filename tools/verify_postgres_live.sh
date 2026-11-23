#!/usr/bin/env bash
# 真实 Postgres 导入测试：需要 docker + postgres:16-alpine（L2 adapter）。
# 没有 docker/镜像时明确 SKIP，不伪造通过。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

if ! command -v docker >/dev/null 2>&1; then
  echo "SKIP: docker 不可用（Postgres adapter 是 L2 可选依赖）"
  exit 0
fi
if ! docker image inspect postgres:16-alpine >/dev/null 2>&1; then
  echo "SKIP: 缺少 postgres:16-alpine 镜像"
  exit 0
fi

NAME="ui-pg-live-$$"
PORT="${UI_PG_PORT:-55439}"
TMP=$(mktemp -d /tmp/ui-pg-live.XXXXXX)
J="$TMP/journal.jsonl"
cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

cat > "$J" <<'EOF'
{"seq":1,"ts":1790000000000,"kind":"turn/start","turn":1,"prompt":"hello","prev":"","hash":"h1"}
{"seq":2,"ts":1790000001000,"kind":"approval/requested","approvalId":"ap-1","callId":"c1","tool":"shell","effects":[],"prev":"h1","hash":"h2"}
{"seq":3,"ts":1790000002000,"kind":"approval/answered","approvalId":"ap-1","callId":"c1","verdict":"deny","prev":"h2","hash":"h3"}
EOF

docker run -d --rm --name "$NAME" \
  -e POSTGRES_PASSWORD=test -e POSTGRES_DB=ui \
  -p "127.0.0.1:${PORT}:5432" postgres:16-alpine >/dev/null

for _ in $(seq 1 60); do
  if docker exec "$NAME" pg_isready -U postgres -d ui >/dev/null 2>&1; then break; fi
  sleep 0.5
done
docker exec "$NAME" pg_isready -U postgres -d ui >/dev/null
docker cp adapters/postgres/schema.sql "$NAME:/tmp/schema.sql" >/dev/null
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -f /tmp/schema.sql >/dev/null

python3 adapters/postgres/export_journal.py \
  --journal "$J" --session default --workspace w1 \
  | docker exec -i "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 >/dev/null
docker exec "$NAME" psql -U postgres -d ui -Atc 'select ui_sync_derived()' >/dev/null

events=$(docker exec "$NAME" psql -U postgres -d ui -Atc 'select count(*) from ui_events')
tasks=$(docker exec "$NAME" psql -U postgres -d ui -Atc 'select count(*) from ui_task_records')
approvals=$(docker exec "$NAME" psql -U postgres -d ui -Atc 'select count(*) from ui_approval_records')
hash=$(docker exec "$NAME" psql -U postgres -d ui -Atc "select hash from ui_events where seq=3")
status=$(docker exec "$NAME" psql -U postgres -d ui -Atc "select status from ui_approval_records where approval_id='ap-1'")
claim1=$(docker exec "$NAME" psql -U postgres -d ui -Atc "select task_seq from ui_claim_task('w1')")
claim2=$(docker exec "$NAME" psql -U postgres -d ui -Atc "select task_seq from ui_claim_task('w2')")
[ "$events" = "3" ] || { echo "FAIL: events=$events"; exit 1; }
[ "$tasks" = "1" ] || { echo "FAIL: tasks=$tasks"; exit 1; }
[ "$approvals" = "1" ] || { echo "FAIL: approvals=$approvals"; exit 1; }
[ "$hash" = "h3" ] || { echo "FAIL: hash=$hash"; exit 1; }
[ "$status" = "denied" ] || { echo "FAIL: approval status=$status"; exit 1; }
[ "$claim1" = "1" ] || { echo "FAIL: first claim=$claim1"; exit 1; }
[ -z "$claim2" ] || { echo "FAIL: task double-claimed=$claim2"; exit 1; }

# append-only：update 必须被触发器拒绝。
if docker exec "$NAME" psql -U postgres -d ui -c "update ui_events set kind='tampered' where seq=1" >/dev/null 2>&1; then
  echo "FAIL: ui_events 允许 update"
  exit 1
fi

echo "PASS postgres live: events=$events tasks=$tasks approvals=$approvals status=$status lease=single-claim append-only=yes"
