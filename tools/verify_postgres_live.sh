#!/usr/bin/env bash
# 真实 Postgres 迁移/lease/优先级/取消测试：需要 docker + postgres:16-alpine。
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
PORT="${UI_PG_PORT:-$(ui_free_port 55439)}"
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

# schema.sql is a psql migration runner; copy the whole adapter so \ir can find
# migrations/*.sql inside the container.
docker cp adapters/postgres "$NAME:/tmp/postgres" >/dev/null

psqlq() {
  docker exec "$NAME" psql -U postgres -d ui -Atc "$1"
}
apply_schema() {
  docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 \
    -f /tmp/postgres/schema.sql >/dev/null
}

apply_schema
apply_schema
apply_schema
versions=$(psqlq 'select count(*) from ui_schema_migrations')
applied=$(psqlq "select string_agg(version, ',' order by version) from ui_schema_migrations")
[ "$versions" = "2" ] || { echo "FAIL: migration version count=$versions"; exit 1; }
[ "$applied" = "001_baseline,002_priority_cancel" ] || { echo "FAIL: migrations=$applied"; exit 1; }

# Import one journal snapshot.  (COPY is intentionally one-shot; the repeatable
# entrypoint tested above is the migration runner, not journal import.)
python3 adapters/postgres/export_journal.py \
  --journal "$J" --session default --workspace w1 \
  | docker exec -i "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null

# Re-run the migration entry against populated tables; data must survive and no
# schema/version error is allowed.
apply_schema
apply_schema
events=$(psqlq 'select count(*) from ui_events')
tasks=$(psqlq 'select count(*) from ui_task_records')
approvals=$(psqlq 'select count(*) from ui_approval_records')
versions2=$(psqlq 'select count(*) from ui_schema_migrations')
hash=$(psqlq 'select hash from ui_events where seq=3')
status=$(psqlq "select status from ui_approval_records where approval_id='ap-1'")
claim1=$(psqlq "select task_seq from ui_claim_task('w1')")
claim2=$(psqlq "select task_seq from ui_claim_task('w2')")
hb1=$(psqlq "select ui_heartbeat_task('w1','default',1,'w1')")
hb2=$(psqlq "select ui_heartbeat_task('w1','default',1,'w2')")
psqlq "update ui_task_records set lease_until=now()-interval '1 second' where task_seq=1" >/dev/null
claim3=$(psqlq "select task_seq from ui_claim_task('w2')")
attempts=$(psqlq "select attempts from ui_task_records where task_seq=1")
[ "$events" = "3" ] || { echo "FAIL: events=$events"; exit 1; }
[ "$tasks" = "1" ] || { echo "FAIL: tasks=$tasks"; exit 1; }
[ "$approvals" = "1" ] || { echo "FAIL: approvals=$approvals"; exit 1; }
[ "$versions2" = "2" ] || { echo "FAIL: versions after rerun=$versions2"; exit 1; }
[ "$hash" = "h3" ] || { echo "FAIL: hash=$hash"; exit 1; }
[ "$status" = "denied" ] || { echo "FAIL: approval status=$status"; exit 1; }
[ "$claim1" = "1" ] || { echo "FAIL: first claim=$claim1"; exit 1; }
[ -z "$claim2" ] || { echo "FAIL: task double-claimed=$claim2"; exit 1; }
[ "$hb1" = "t" ] || { echo "FAIL: owner heartbeat=$hb1"; exit 1; }
[ "$hb2" = "f" ] || { echo "FAIL: non-owner heartbeat=$hb2"; exit 1; }
[ "$claim3" = "1" ] || { echo "FAIL: expired lease not reclaimable=$claim3"; exit 1; }
[ "$attempts" = "2" ] || { echo "FAIL: attempts=$attempts"; exit 1; }

# append-only：update 必须被触发器拒绝（迁移重复执行后触发器仍生效）。
if docker exec "$NAME" psql -U postgres -d ui -c "update ui_events set kind='tampered' where seq=1" >/dev/null 2>&1; then
  echo "FAIL: ui_events 允许 update"
  exit 1
fi

# ---------- priority ordering: priority DESC, created_at ASC ----------
psqlq "update ui_task_records set status='done', finished_at=now() where task_seq=1" >/dev/null
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c "
insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
 ('w1','prio',1,'turn/start','{\"turn\":1,\"prompt\":\"priority-low\"}','','p1',now()-interval '4 minutes'),
 ('w1','prio',2,'turn/start','{\"turn\":2,\"prompt\":\"priority-high-newer\"}','','p2',now()-interval '2 minutes'),
 ('w1','prio',3,'turn/start','{\"turn\":3,\"prompt\":\"priority-high-older\"}','','p3',now()-interval '3 minutes')" >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null
psqlq "update ui_task_records set priority=5 where session_id='prio' and task_seq=1" >/dev/null
psqlq "update ui_task_records set priority=9 where session_id='prio' and task_seq=2" >/dev/null
psqlq "update ui_task_records set priority=9 where session_id='prio' and task_seq=3" >/dev/null

claim_prio() {
  psqlq "select c.task_seq from ui_claim_task('$1') c where c.session_id='prio'"
}
finish_prio() {
  psqlq "update ui_task_records set status='done', finished_at=now() where session_id='prio' and task_seq=$1" >/dev/null
}
p1=$(claim_prio prio-a)
finish_prio "$p1"
p2=$(claim_prio prio-b)
finish_prio "$p2"
p3=$(claim_prio prio-c)
finish_prio "$p3"
[ "$p1" = "3" ] || { echo "FAIL: highest priority/oldest claim=$p1 (want 3)"; exit 1; }
[ "$p2" = "2" ] || { echo "FAIL: second priority claim=$p2 (want 2)"; exit 1; }
[ "$p3" = "1" ] || { echo "FAIL: low priority claim=$p3 (want 1)"; exit 1; }

# ---------- queued cancel ----------
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c "
insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
 ('w1','prio',4,'turn/start','{\"turn\":4,\"prompt\":\"cancel queued\"}','','p4',now())" >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null
psqlq "update ui_task_records set priority=100 where session_id='prio' and task_seq=4" >/dev/null
cancel_queued=$(psqlq "select ui_cancel_task('w1','prio',4)")
status_queued=$(psqlq "select status from ui_task_records where session_id='prio' and task_seq=4")
flag_queued=$(psqlq "select cancel_requested::int from ui_task_records where session_id='prio' and task_seq=4")
[ "$cancel_queued" = "cancelled" ] || { echo "FAIL: queued cancel returned=$cancel_queued"; exit 1; }
[ "$status_queued" = "cancelled" ] || { echo "FAIL: queued cancel status=$status_queued"; exit 1; }
[ "$flag_queued" = "1" ] || { echo "FAIL: queued cancel flag=$flag_queued"; exit 1; }
claim_after_cancel=$(psqlq "select coalesce(max(c.task_seq),0) from ui_claim_task('prio-d') c")
[ "$claim_after_cancel" = "0" ] || { echo "FAIL: cancelled queued task claimed=$claim_after_cancel"; exit 1; }

# A late turn/end must not turn a cancelled task into done.
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c "
insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
 ('w1','prio',5,'turn/end','{\"turn\":4}','','p5',now())" >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null
status_after_end=$(psqlq "select status from ui_task_records where session_id='prio' and task_seq=4")
[ "$status_after_end" = "cancelled" ] || { echo "FAIL: turn/end overwrote cancelled=$status_after_end"; exit 1; }

# ---------- running cancel requests a flag but keeps running ----------
docker exec "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -c "
insert into ui_events(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values
 ('w1','prio',6,'turn/start','{\"turn\":5,\"prompt\":\"cancel running\"}','','p6',now())" >/dev/null
psqlq 'select ui_sync_derived()' >/dev/null
claim_running=$(claim_prio prio-e)
[ "$claim_running" = "6" ] || { echo "FAIL: running-cancel claim=$claim_running"; exit 1; }
cancel_running=$(psqlq "select ui_cancel_task('w1','prio',6)")
status_running=$(psqlq "select status from ui_task_records where session_id='prio' and task_seq=6")
flag_running=$(psqlq "select cancel_requested::int from ui_task_records where session_id='prio' and task_seq=6")
[ "$cancel_running" = "running" ] || { echo "FAIL: running cancel returned=$cancel_running"; exit 1; }
[ "$status_running" = "running" ] || { echo "FAIL: running cancel status=$status_running"; exit 1; }
[ "$flag_running" = "1" ] || { echo "FAIL: running cancel flag=$flag_running"; exit 1; }
claim_while_leased=$(psqlq "select coalesce(max(c.task_seq),0) from ui_claim_task('prio-f') c")
[ "$claim_while_leased" = "0" ] || { echo "FAIL: running/cancelled task re-claimed=$claim_while_leased"; exit 1; }

echo "PASS postgres live: migrations=2 rerun=yes events=$events tasks=$tasks approvals=$approvals status=$status lease=claim+heartbeat+expiry priority=3,2,1 queued_cancel=cancelled running_cancel=requested append-only=yes"
