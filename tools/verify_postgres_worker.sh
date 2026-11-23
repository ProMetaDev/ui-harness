#!/usr/bin/env bash
# L2 worker live 测试：真实 Postgres + 假 ui 可执行文件。
# 验证 claim -> run -> status=done/failed 以及两条取消路径，不验证模型/工具。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

if ! command -v docker >/dev/null 2>&1 || ! docker image inspect postgres:16-alpine >/dev/null 2>&1; then
  echo "SKIP: docker/postgres:16-alpine 不可用"
  exit 0
fi

NAME="ui-pg-worker-$$"
PORT="${UI_PG_WORKER_PORT:-$(ui_free_port 55441)}"
TMP=$(mktemp -d /tmp/ui-pg-worker.XXXXXX)
cleanup() {
  [ -n "${WPID:-}" ] && kill "$WPID" >/dev/null 2>&1 || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

psqlq() {
  docker exec "$NAME" psql -U postgres -d ui -Atc "$1"
}
run_worker() {
  local ui="$1"
  shift
  python3 adapters/postgres/worker.py \
    --psql "$PSQL" --dsn ignored --worker w1 \
    --ui "$ui" --workspace "$TMP/ws" --once "$@"
}

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

# schema.sql is a psql migration runner; copy the whole adapter so \ir can find
# migrations/*.sql inside the container.
docker cp adapters/postgres "$NAME:/tmp/postgres" >/dev/null
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 \
  -f /tmp/postgres/schema.sql >/dev/null
# Apply twice: worker must run on a database whose migration entry was re-run.
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 \
  -f /tmp/postgres/schema.sql >/dev/null
versions=$(psqlq 'select count(*) from ui_schema_migrations')
[ "$versions" = "2" ] || { echo "FAIL: migration versions=$versions"; exit 1; }

python3 adapters/postgres/export_journal.py --journal "$J" --session default --workspace w1 \
  | docker exec -i "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null

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
STARTED_FILE="$TMP/started.txt"
export ARGS_FILE STARTED_FILE

# 基本路径：claim -> fake ui -> done。
run_worker "$FAKE"
grep -q -- "--task" "$ARGS_FILE" || { echo "FAIL: worker did not pass --task"; cat "$ARGS_FILE"; exit 1; }
grep -q "hello worker" "$ARGS_FILE" || { echo "FAIL: prompt missing"; cat "$ARGS_FILE"; exit 1; }
status=$(psqlq "select status from ui_task_records where task_seq=1")
[ "$status" = "done" ] || { echo "FAIL: status=$status"; exit 1; }

# 失败路径：假 ui 退出 2，worker 必须写 failed，而不是 done。
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c \
  "insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
   ('w1','default',2,'turn/start','{\"turn\":2,\"prompt\":\"fail me\"}','h1','h2',now())" >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null
FAKE_FAIL="$TMP/fake-fail"
printf '#!/bin/sh\nexit 2\n' > "$FAKE_FAIL"
chmod +x "$FAKE_FAIL"
run_worker "$FAKE_FAIL"
status2=$(psqlq "select status from ui_task_records where task_seq=2")
[ "$status2" = "failed" ] || { echo "FAIL: failed path status=$status2"; exit 1; }

# 取消路径 1：任务已 running 且 cancel_requested，worker 在 claim 前发现并标
# cancelled，绝不能启动 ui，也不能写 done。
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c \
  "insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
   ('w1','default',3,'turn/start','{\"turn\":3,\"prompt\":\"cancel before run\"}','h2','h3',now())" >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null
claimed=$(psqlq "select task_seq from ui_claim_task('w1')")
[ "$claimed" = "3" ] || { echo "FAIL: pre-run cancel claim=$claimed"; exit 1; }
cancelled=$(psqlq "select ui_cancel_task('w1','default',3)")
[ "$cancelled" = "running" ] || { echo "FAIL: pre-run cancel returned=$cancelled"; exit 1; }
rm -f "$ARGS_FILE"
run_worker "$FAKE"
status3=$(psqlq "select status from ui_task_records where task_seq=3")
[ "$status3" = "cancelled" ] || { echo "FAIL: pre-run cancel status=$status3"; exit 1; }
[ ! -e "$ARGS_FILE" ] || { echo "FAIL: worker ran a task already cancellation-requested"; exit 1; }

# 取消路径 2：worker 正在运行 fake ui，取消请求由 heartbeat 发现，worker 终止
# 子进程并标 cancelled；输出不能是 done。
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c \
  "insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
   ('w1','default',4,'turn/start','{\"turn\":4,\"prompt\":\"cancel in flight\"}','h3','h4',now())" >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null
FAKE_SLOW="$TMP/fake-slow"
cat > "$FAKE_SLOW" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" > "$ARGS_FILE"
: > "$STARTED_FILE"
exec sleep 30
EOF
chmod +x "$FAKE_SLOW"
rm -f "$ARGS_FILE" "$STARTED_FILE"
run_worker "$FAKE_SLOW" >"$TMP/worker-cancel.out" 2>&1 &
WPID=$!
for _ in $(seq 1 100); do
  [ -f "$STARTED_FILE" ] && break
  sleep 0.1
done
[ -f "$STARTED_FILE" ] || { echo "FAIL: slow ui did not start"; cat "$TMP/worker-cancel.out"; exit 1; }
status_running=$(psqlq "select status from ui_task_records where task_seq=4")
[ "$status_running" = "running" ] || { echo "FAIL: in-flight status=$status_running"; exit 1; }
cancelled2=$(psqlq "select ui_cancel_task('w1','default',4)")
[ "$cancelled2" = "running" ] || { echo "FAIL: in-flight cancel returned=$cancelled2"; exit 1; }
if ! wait "$WPID"; then
  echo "FAIL: worker failed during in-flight cancellation"
  cat "$TMP/worker-cancel.out"
  exit 1
fi
WPID=""
status4=$(psqlq "select status from ui_task_records where task_seq=4")
flag4=$(psqlq "select cancel_requested::int from ui_task_records where task_seq=4")
[ "$status4" = "cancelled" ] || { echo "FAIL: in-flight cancel status=$status4"; exit 1; }
[ "$flag4" = "1" ] || { echo "FAIL: in-flight cancel flag=$flag4"; exit 1; }
grep -q "cancelled task=4" "$TMP/worker-cancel.out" || {
  echo "FAIL: worker did not report cancellation"; cat "$TMP/worker-cancel.out"; exit 1;
}

# 真实 ui run：worker 必须跑真实 journal，并把新事件增量同步回 Postgres。
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c \
  "insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
   ('w1','real',1,'turn/start','{\"turn\":1,\"prompt\":\"real worker\"}','','h-real-1',now())" >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null
UIWS="$TMP/ui-ws"
mkdir -p "$UIWS"
SC="$TMP/scenario.json"
printf '%s\n' '{"model":"scripted","steps":[{"content":"real worker done"}]}' > "$SC"
python3 adapters/postgres/worker.py \
  --psql "$PSQL" --dsn ignored --worker w1 \
  --ui "$UI_BIN" --workspace "$UIWS" --sync-events --once -- --mock "$SC" --session real
status5=$(psqlq "select status from ui_task_records where session_id='real' and task_seq=1")
[ "$status5" = "done" ] || { echo "FAIL: real ui status=$status5"; exit 1; }
[ -f "$UIWS/.ui/sessions/real.jsonl" ] || { echo "FAIL: real ui did not write session journal"; exit 1; }
synced=$(psqlq "select count(*) from ui_events where session_id='real'")
[ "$synced" -ge 3 ] || { echo "FAIL: synced events=$synced"; exit 1; }
echo "PASS postgres worker: migrations rerun + fake done/failed + pre-run cancel + in-flight cancel + real ui run + journal sync"
