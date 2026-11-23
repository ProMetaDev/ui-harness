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

# 失败路径：假 ui 退出 2，worker 必须写 failed，而不是 done。
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c \
  "insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
   ('w1','default',2,'turn/start','{\"turn\":2,\"prompt\":\"fail me\"}','h1','h2',now())" >/dev/null
docker exec "$NAME" psql -U postgres -d ui -Atc 'select ui_sync_derived()' >/dev/null
FAKE_FAIL="$TMP/fake-fail"
printf '#!/bin/sh\nexit 2\n' > "$FAKE_FAIL"
chmod +x "$FAKE_FAIL"
python3 adapters/postgres/worker.py \
  --psql "$PSQL" --dsn ignored --worker w1 \
  --ui "$FAKE_FAIL" --workspace "$TMP/ws" --once
status2=$(docker exec "$NAME" psql -U postgres -d ui -Atc "select status from ui_task_records where task_seq=2")
[ "$status2" = "failed" ] || { echo "FAIL: failed path status=$status2"; exit 1; }

# 真实 ui run：worker 必须跑真实 journal，并把新事件增量同步回 Postgres。
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c \
  "insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
   ('w1','real',1,'turn/start','{\"turn\":1,\"prompt\":\"real worker\"}','','h-real-1',now())" >/dev/null
docker exec "$NAME" psql -U postgres -d ui -Atc 'select ui_sync_derived()' >/dev/null
UIWS="$TMP/ui-ws"
mkdir -p "$UIWS"
SC="$TMP/scenario.json"
printf '%s\n' '{"model":"scripted","steps":[{"content":"real worker done"}]}' > "$SC"
python3 adapters/postgres/worker.py \
  --psql "$PSQL" --dsn ignored --worker w1 \
  --ui "$UI_BIN" --workspace "$UIWS" --sync-events --once -- --mock "$SC" --session real
status3=$(docker exec "$NAME" psql -U postgres -d ui -Atc "select status from ui_task_records where session_id='real' and task_seq=1")
[ "$status3" = "done" ] || { echo "FAIL: real ui status=$status3"; exit 1; }
[ -f "$UIWS/.ui/sessions/real.jsonl" ] || { echo "FAIL: real ui did not write session journal"; exit 1; }
synced=$(docker exec "$NAME" psql -U postgres -d ui -Atc "select count(*) from ui_events where session_id='real'")
[ "$synced" -ge 3 ] || { echo "FAIL: synced events=$synced"; exit 1; }
echo "PASS postgres worker: fake done/failed + real ui run + journal sync"
