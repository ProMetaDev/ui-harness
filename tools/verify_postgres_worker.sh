#!/usr/bin/env bash
# L2 worker live 测试：真实 Postgres + 假 ui 可执行文件。
# 验证 claim -> run -> status=done 的协议，不验证模型/工具。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

if ! command -v docker >/dev/null 2>&1 || ! docker image inspect postgres:16-alpine >/dev/null 2>&1; then
  echo "SKIP: docker/postgres:16-alpine 不可用"
  exit 0
fi

NAME="ui-pg-worker-$$"
PORT="${UI_PG_WORKER_PORT:-55441}"
TMP=$(mktemp -d /tmp/ui-pg-worker.XXXXXX)
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$TMP"; }
trap cleanup EXIT

J="$TMP/journal.jsonl"
cat > "$J" <<'EOF'
{"seq":1,"ts":1790000000000,"kind":"turn/start","turn":1,"prompt":"hello worker","prev":"","hash":"h1"}
EOF

docker run -d --rm --name "$NAME" \
  -e POSTGRES_PASSWORD=test -e POSTGRES_DB=ui \
  -p "127.0.0.1:${PORT}:5432" postgres:16-alpine >/dev/null
for _ in $(seq 1 60); do
  docker exec "$NAME" pg_isready -U postgres -d ui >/dev/null 2>&1 && break
  sleep 0.5
done
docker exec "$NAME" pg_isready -U postgres -d ui >/dev/null
docker cp adapters/postgres/schema.sql "$NAME:/tmp/schema.sql" >/dev/null
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -f /tmp/schema.sql >/dev/null
python3 adapters/postgres/export_journal.py --journal "$J" --session default --workspace w1 \
  | docker exec -i "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 >/dev/null
docker exec "$NAME" psql -U postgres -d ui -Atc 'select ui_sync_derived()' >/dev/null

# worker 需要一个 psql 命令；用 wrapper 把请求转发进容器。
PSQL="$TMP/psql"
cat > "$PSQL" <<EOF
#!/bin/sh
shift
exec docker exec -i "$NAME" psql -U postgres -d ui "\$@"
EOF
chmod +x "$PSQL"

FAKE="$TMP/fake-ui"
cat > "$FAKE" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" > "$ARGS_FILE"
exit 0
EOF
chmod +x "$FAKE"
ARGS_FILE="$TMP/args.txt"
export ARGS_FILE

python3 adapters/postgres/worker.py \
  --psql "$PSQL" --dsn ignored --worker w1 \
  --ui "$FAKE" --workspace "$TMP/ws" --once

grep -q -- "--task" "$ARGS_FILE" || { echo "FAIL: worker did not pass --task"; cat "$ARGS_FILE"; exit 1; }
grep -q "hello worker" "$ARGS_FILE" || { echo "FAIL: prompt missing"; cat "$ARGS_FILE"; exit 1; }
status=$(docker exec "$NAME" psql -U postgres -d ui -Atc "select status from ui_task_records where task_seq=1")
[ "$status" = "done" ] || { echo "FAIL: status=$status"; exit 1; }
echo "PASS postgres worker: claim -> fake ui -> status=done"
