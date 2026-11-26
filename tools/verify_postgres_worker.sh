#!/usr/bin/env bash
# L2 worker live 测试：真实 Postgres + 假 ui 可执行文件（+ 一次真实 ui run）。
#
# 覆盖：
#   - claim -> run -> done / failed / cancelled（保留原有三条测试路径）
#   - workspace 映射：root + workspace_id、registry、越界/缺映射 fail closed
#   - prompt 含 tab/换行时 JSON claim 不错位
#   - 非 default session 的 journal 同步
#   - heartbeat 丢租约 -> 终止本地 ui、写 failed、不写 done
#   - 大批量事件用 psql stdin 同步（不依赖命令行长度）
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
ROOT="$TMP/wsroot"
mkdir -p "$ROOT"
cleanup() {
  [ -n "${WPID:-}" ] && kill "$WPID" >/dev/null 2>&1 || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

psqlq() {
  docker exec "$NAME" psql -U postgres -d ui -Atc "$1"
}
psql_stdin() {
  docker exec -i "$NAME" psql -U postgres -d ui -v ON_ERROR_STOP=1 -q
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
[ "$versions" = "3" ] || { echo "FAIL: migration versions=$versions"; exit 1; }

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

# fake ui 把 argv 记成 JSON：这样断言的是"一个参数"，不会被换行/制表符骗过。
FAKE="$TMP/fake-ui"
cat > "$FAKE" <<'EOF'
#!/bin/sh
python3 - "$ARGS_FILE" "$@" <<'PY'
import json
import sys
open(sys.argv[1], "w", encoding="utf-8").write(json.dumps(sys.argv[2:], ensure_ascii=False))
PY
EOF
chmod +x "$FAKE"
ARGS_FILE="$TMP/args.json"
STARTED_FILE="$TMP/started.txt"
PID_FILE="$TMP/child.pid"
export ARGS_FILE STARTED_FILE PID_FILE

run_worker() {
  local ui="$1"
  shift
  python3 adapters/postgres/worker.py \
    --psql "$PSQL" --dsn ignored --worker w1 \
    --ui "$ui" --workspace-root "$ROOT" --once "$@"
}
arg_value() {
  python3 - "$ARGS_FILE" "$1" <<'PY'
import json
import sys
args = json.load(open(sys.argv[1], encoding="utf-8"))
print(args[args.index(sys.argv[2]) + 1] if sys.argv[2] in args else "")
PY
}
assert_prompt() {
  python3 - "$ARGS_FILE" "$1" <<'PY'
import json
import sys
args = json.load(open(sys.argv[1], encoding="utf-8"))
prompt = args[args.index("--task") + 1]
if prompt != sys.argv[2]:
    raise SystemExit("prompt mismatch: %r != %r" % (prompt, sys.argv[2]))
PY
}
add_task() {
  python3 - "$1" "$2" "$3" "$4" <<'PY' | psql_stdin
import json
import sys
ws, session, seq, prompt = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
payload = json.dumps({"turn": 1, "prompt": prompt}, ensure_ascii=False)
def lit(v):
    return "'" + str(v).replace("'", "''") + "'"
row = [
    lit(ws), lit(session), str(seq), lit("turn/start"), lit(payload),
    lit(""), lit("hash-%s-%s-%s" % (ws, session, seq)), "now()",
]
print("insert into ui_events "
      "(workspace_id,session_id,seq,kind,payload,prev_hash,hash,created_at) values ("
      + ",".join(row) + ");")
print("select ui_sync_derived();")
PY
}
wait_started() {
  for _ in $(seq 1 100); do
    [ -f "$STARTED_FILE" ] && return 0
    sleep 0.1
  done
  return 1
}
task_status() {
  psqlq "select status from ui_task_records where workspace_id=$1 and session_id=$2 and task_seq=$3"
}

# 基本路径：claim -> fake ui -> done；工作区必须是 root/workspace_id。
# 这条任务来自上面 export_journal.py 的 COPY 导入。
run_worker "$FAKE"
[ -f "$ARGS_FILE" ] || { echo "FAIL: worker did not run fake ui"; exit 1; }
ws_arg=$(arg_value --workspace)
[ "$ws_arg" = "$ROOT/w1" ] || { echo "FAIL: --workspace=$ws_arg (want $ROOT/w1)"; exit 1; }
session_arg=$(arg_value --session)
[ "$session_arg" = "default" ] || { echo "FAIL: --session=$session_arg"; exit 1; }
assert_prompt "hello worker"
status=$(task_status "'w1'" "'default'" 1)
[ "$status" = "done" ] || { echo "FAIL: status=$status"; exit 1; }

# 失败路径：假 ui 退出 2，worker 必须写 failed，而不是 done。
add_task w1 default 2 "fail me"
FAKE_FAIL="$TMP/fake-fail"
printf '#!/bin/sh\nexit 2\n' > "$FAKE_FAIL"
chmod +x "$FAKE_FAIL"
run_worker "$FAKE_FAIL"
status2=$(task_status "'w1'" "'default'" 2)
[ "$status2" = "failed" ] || { echo "FAIL: failed path status=$status2"; exit 1; }
err2=$(psqlq "select last_error from ui_task_records where workspace_id='w1' and session_id='default' and task_seq=2")
case "$err2" in *"exit"*) ;; *) echo "FAIL: failed path last_error=$err2"; exit 1 ;; esac

# prompt 含 tab/换行：JSON claim 必须原样传给 ui（旧的 tab 分隔会错位）。
add_task w1 tabs 1 $'line-1\tindent\nline-2 with "quote" and \\backslash'
run_worker "$FAKE"
status_tabs=$(task_status "'w1'" "'tabs'" 1)
[ "$status_tabs" = "done" ] || { echo "FAIL: tab prompt status=$status_tabs"; exit 1; }
[ "$(arg_value --session)" = "tabs" ] || { echo "FAIL: tab prompt session mismatch"; exit 1; }
assert_prompt $'line-1\tindent\nline-2 with "quote" and \\backslash'

# workspace registry：显式映射到 root 下的登记路径。
REG="$TMP/registry.json"
printf '%s\n' '{"reg":"repos/reg"}' > "$REG"
add_task reg default 1 "registry task"
run_worker "$FAKE" --workspace-registry "$REG"
[ "$(arg_value --workspace)" = "$ROOT/repos/reg" ] \
  || { echo "FAIL: registry workspace=$(arg_value --workspace)"; exit 1; }
[ "$(task_status "'reg'" "'default'" 1)" = "done" ] \
  || { echo "FAIL: registry task not done"; exit 1; }

# workspace_id 越界：相对 `..` 和绝对路径都必须 fail closed，且不能启动 ui。
rm -f "$ARGS_FILE"
add_task "../escape" default 1 "escape attempt"
run_worker "$FAKE"
[ ! -e "$ARGS_FILE" ] || { echo "FAIL: worker ran ui for ../escape"; exit 1; }
status_esc=$(task_status "'../escape'" "'default'" 1)
[ "$status_esc" = "failed" ] || { echo "FAIL: ../escape status=$status_esc"; exit 1; }
err_esc=$(psqlq "select last_error from ui_task_records where workspace_id='../escape' and session_id='default'")
case "$err_esc" in *"mapping refused"*) ;; *) echo "FAIL: ../escape last_error=$err_esc"; exit 1 ;; esac
[ ! -e "$TMP/escape" ] || { echo "FAIL: ../escape created a directory outside root"; exit 1; }

rm -f "$ARGS_FILE"
add_task "/abs" default 1 "absolute attempt"
run_worker "$FAKE"
[ ! -e "$ARGS_FILE" ] || { echo "FAIL: worker ran ui for absolute workspace"; exit 1; }
status_abs=$(task_status "'/abs'" "'default'" 1)
[ "$status_abs" = "failed" ] || { echo "FAIL: /abs status=$status_abs"; exit 1; }

# 非法 session：ui 会静默退回 default，worker 必须先拒绝。
rm -f "$ARGS_FILE"
add_task w1 "bad/session" 1 "bad session"
run_worker "$FAKE"
[ ! -e "$ARGS_FILE" ] || { echo "FAIL: worker ran ui for invalid session"; exit 1; }
status_sess=$(task_status "'w1'" "'bad/session'" 1)
[ "$status_sess" = "failed" ] || { echo "FAIL: invalid session status=$status_sess"; exit 1; }
err_sess=$(psqlq "select last_error from ui_task_records where workspace_id='w1' and session_id='bad/session'")
case "$err_sess" in *"invalid session"*) ;; *) echo "FAIL: invalid session last_error=$err_sess"; exit 1 ;; esac

# 单工作区模式缺映射：只服务 default，claim 到 w1 必须失败而不是"就近"跑。
SINGLE="$TMP/single-ws"
mkdir -p "$SINGLE"
add_task w1 single 1 "pinned workspace"
rm -f "$ARGS_FILE"
python3 adapters/postgres/worker.py \
  --psql "$PSQL" --dsn ignored --worker w1 \
  --ui "$FAKE" --workspace "$SINGLE" --once >"$TMP/single.out" 2>&1
[ ! -e "$ARGS_FILE" ] || { echo "FAIL: single-workspace mode ran wrong workspace"; exit 1; }
status_single=$(task_status "'w1'" "'single'" 1)
[ "$status_single" = "failed" ] || { echo "FAIL: single-workspace status=$status_single"; exit 1; }
err_single=$(psqlq "select last_error from ui_task_records where workspace_id='w1' and session_id='single'")
case "$err_single" in *"no workspace mapping"*) ;; *) echo "FAIL: single-workspace last_error=$err_single"; exit 1 ;; esac
[ ! -e "$SINGLE/w1" ] || { echo "FAIL: single-workspace mode created a sibling workspaces dir"; exit 1; }

# 取消路径 1：任务已 running 且 cancel_requested，worker 在 claim 前发现并标
# cancelled，绝不能启动 ui，也不能写 done。
add_task w1 default 3 "cancel before run"
claimed=$(psqlq "select task_seq from ui_claim_task('w1')")
[ "$claimed" = "3" ] || { echo "FAIL: pre-run cancel claim=$claimed"; exit 1; }
cancelled=$(psqlq "select ui_cancel_task('w1','default',3)")
[ "$cancelled" = "running" ] || { echo "FAIL: pre-run cancel returned=$cancelled"; exit 1; }
rm -f "$ARGS_FILE"
run_worker "$FAKE"
status3=$(task_status "'w1'" "'default'" 3)
[ "$status3" = "cancelled" ] || { echo "FAIL: pre-run cancel status=$status3"; exit 1; }
[ ! -e "$ARGS_FILE" ] || { echo "FAIL: worker ran a task already cancellation-requested"; exit 1; }

# 取消路径 2：worker 正在运行 fake ui，取消请求由 heartbeat 发现，worker 终止
# 子进程并标 cancelled；输出不能是 done。
add_task w1 default 4 "cancel in flight"
FAKE_SLOW="$TMP/fake-slow"
cat > "$FAKE_SLOW" <<'EOF'
#!/bin/sh
python3 - "$ARGS_FILE" "$@" <<'PY'
import json
import sys
open(sys.argv[1], "w", encoding="utf-8").write(json.dumps(sys.argv[2:], ensure_ascii=False))
PY
: > "$STARTED_FILE"
echo $$ > "$PID_FILE"
exec sleep 30
EOF
chmod +x "$FAKE_SLOW"
rm -f "$ARGS_FILE" "$STARTED_FILE" "$PID_FILE"
run_worker "$FAKE_SLOW" >"$TMP/worker-cancel.out" 2>&1 &
WPID=$!
wait_started || { echo "FAIL: slow ui did not start"; cat "$TMP/worker-cancel.out"; exit 1; }
status_running=$(task_status "'w1'" "'default'" 4)
[ "$status_running" = "running" ] || { echo "FAIL: in-flight status=$status_running"; exit 1; }
cancelled2=$(psqlq "select ui_cancel_task('w1','default',4)")
[ "$cancelled2" = "running" ] || { echo "FAIL: in-flight cancel returned=$cancelled2"; exit 1; }
if ! wait "$WPID"; then
  echo "FAIL: worker failed during in-flight cancellation"
  cat "$TMP/worker-cancel.out"
  exit 1
fi
WPID=""
status4=$(task_status "'w1'" "'default'" 4)
flag4=$(psqlq "select cancel_requested::int from ui_task_records where task_seq=4")
[ "$status4" = "cancelled" ] || { echo "FAIL: in-flight cancel status=$status4"; exit 1; }
[ "$flag4" = "1" ] || { echo "FAIL: in-flight cancel flag=$flag4"; exit 1; }
grep -q "cancelled task=4" "$TMP/worker-cancel.out" || {
  echo "FAIL: worker did not report cancellation"; cat "$TMP/worker-cancel.out"; exit 1;
}

# 真实 ui run：worker 必须用 claim 里的 session 跑真实 journal，并把新事件
# 增量同步回 Postgres（非 default session）。
add_task w1 real 1 "real worker"
SC="$TMP/scenario.json"
printf '%s\n' '{"model":"scripted","steps":[{"content":"real worker done"}]}' > "$SC"
python3 adapters/postgres/worker.py \
  --psql "$PSQL" --dsn ignored --worker w1 \
  --ui "$UI_BIN" --workspace-root "$ROOT" --sync-events --once -- --mock "$SC"
status5=$(task_status "'w1'" "'real'" 1)
[ "$status5" = "done" ] || { echo "FAIL: real ui status=$status5"; exit 1; }
[ -f "$ROOT/w1/.ui/sessions/real.jsonl" ] || { echo "FAIL: real ui did not write session journal"; exit 1; }
synced=$(psqlq "select count(*) from ui_events where workspace_id='w1' and session_id='real'")
[ "$synced" -ge 3 ] || { echo "FAIL: synced real events=$synced"; exit 1; }

# 大批量 sync：journal 6MB+，SQL 只能走 stdin（旧的 psql -c 会撞命令行上限）。
add_task w1 bulk 1 "bulk sync"
mkdir -p "$ROOT/w1/.ui/sessions"
python3 - "$ROOT/w1/.ui/sessions/bulk.jsonl" 1500 <<'PY'
import json
import sys
path, n = sys.argv[1], int(sys.argv[2])
with open(path, "w", encoding="utf-8") as f:
    for i in range(1, n + 1):
        if i == 1:
            payload = {"turn": 1, "prompt": "bulk sync"}
            kind = "turn/start"
        else:
            payload = {"data": "x" * 4000, "n": i}
            kind = "tool/result"
        f.write(json.dumps({
            "seq": i,
            "ts": 1790000000000 + i,
            "kind": kind,
            "turn": 1,
            "payload": payload,
            "prev": "" if i == 1 else "bulk-%d" % (i - 1),
            "hash": "bulk-%d" % i,
        }, ensure_ascii=False) + "\n")
PY
bulk_bytes=$(wc -c < "$ROOT/w1/.ui/sessions/bulk.jsonl")
[ "$bulk_bytes" -gt 2000000 ] || { echo "FAIL: bulk journal too small ($bulk_bytes)"; exit 1; }
run_worker "$FAKE" --sync-events
status_bulk=$(task_status "'w1'" "'bulk'" 1)
[ "$status_bulk" = "done" ] || { echo "FAIL: bulk task status=$status_bulk"; exit 1; }
[ "$(arg_value --session)" = "bulk" ] || { echo "FAIL: bulk session mismatch"; exit 1; }
bulk_count=$(psqlq "select count(*) from ui_events where workspace_id='w1' and session_id='bulk'")
[ "$bulk_count" = "1500" ] || { echo "FAIL: bulk synced=$bulk_count bytes=$bulk_bytes"; exit 1; }

# lease_lost（任务被别的 worker 接管）：heartbeat 发现后必须终止本地 ui，
# 不再续租，也不写 done。
add_task w1 lease 1 "lease takeover"
rm -f "$ARGS_FILE" "$STARTED_FILE" "$PID_FILE"
run_worker "$FAKE_SLOW" >"$TMP/worker-lease.out" 2>&1 &
WPID=$!
wait_started || { echo "FAIL: takeover ui did not start"; cat "$TMP/worker-lease.out"; exit 1; }
psqlq "update ui_task_records set worker_id='w2' where workspace_id='w1' and session_id='lease' and task_seq=1" >/dev/null
if ! wait "$WPID"; then
  echo "FAIL: worker failed during lease takeover"
  cat "$TMP/worker-lease.out"
  exit 1
fi
WPID=""
grep -q "lease lost" "$TMP/worker-lease.out" || {
  echo "FAIL: worker did not report lease loss"; cat "$TMP/worker-lease.out"; exit 1;
}
child_pid=$(cat "$PID_FILE")
if kill -0 "$child_pid" 2>/dev/null; then
  echo "FAIL: worker left ui child running after lease loss"
  kill "$child_pid" >/dev/null 2>&1 || true
  exit 1
fi
status_takeover=$(task_status "'w1'" "'lease'" 1)
[ "$status_takeover" = "running" ] || { echo "FAIL: takeover status=$status_takeover (want running/w2, never done)"; exit 1; }
psqlq "update ui_task_records set status='failed', finished_at=now() where workspace_id='w1' and session_id='lease'" >/dev/null
psqlq "delete from ui_workspace_leases" >/dev/null

# lease_lost（heartbeat 自己返回 false）：worker 仍持有任务，必须写 failed
# 并记录原因，不能写 done。用测试副本里的函数覆盖 + 开关表制造确定性失败：
# 先让 heartbeat 正常返回 true（保证 ui 真的启动），再翻成 false。
psql_stdin <<'SQL'
create table ui_test_heartbeat (ok boolean not null);
insert into ui_test_heartbeat values (true);
create or replace function ui_heartbeat_task(
  p_workspace text, p_session text, p_task_seq bigint, p_worker text
) returns boolean language sql as $$ select ok from ui_test_heartbeat limit 1 $$;
SQL
add_task w1 lease2 1 "heartbeat rejected"
rm -f "$ARGS_FILE" "$STARTED_FILE" "$PID_FILE"
run_worker "$FAKE_SLOW" >"$TMP/worker-hb.out" 2>&1 &
WPID=$!
wait_started || { echo "FAIL: lease-lost ui did not start"; cat "$TMP/worker-hb.out"; exit 1; }
psqlq "update ui_test_heartbeat set ok=false" >/dev/null
if ! wait "$WPID"; then
  echo "FAIL: worker failed during lease_lost"
  cat "$TMP/worker-hb.out"
  exit 1
fi
WPID=""
status_lost=$(task_status "'w1'" "'lease2'" 1)
[ "$status_lost" = "failed" ] || { echo "FAIL: lease_lost status=$status_lost"; exit 1; }
err_lost=$(psqlq "select last_error from ui_task_records where workspace_id='w1' and session_id='lease2'")
case "$err_lost" in *"lease lost"*) ;; *) echo "FAIL: lease_lost last_error=$err_lost"; exit 1 ;; esac
child_pid=$(cat "$PID_FILE")
if kill -0 "$child_pid" 2>/dev/null; then
  echo "FAIL: worker left ui child running after heartbeat rejection"
  kill "$child_pid" >/dev/null 2>&1 || true
  exit 1
fi

# registry 本身不安全时在启动阶段 fail closed（不是运行到一半才发现）。
BADREG="$TMP/bad-registry.json"
for bad in '{"x":"/tmp/escape"}' '{"x":"../escape"}'; do
  printf '%s\n' "$bad" > "$BADREG"
  if python3 adapters/postgres/worker.py \
      --psql "$PSQL" --dsn ignored --worker w1 \
      --ui "$FAKE" --workspace-root "$ROOT" \
      --workspace-registry "$BADREG" --once >"$TMP/badreg.out" 2>&1; then
    echo "FAIL: unsafe workspace registry accepted: $bad"
    exit 1
  fi
  grep -q "不合法" "$TMP/badreg.out" || {
    echo "FAIL: unsafe registry did not fail with a clear error: $bad"
    cat "$TMP/badreg.out"
    exit 1
  }
done

echo "PASS postgres worker: migrations=3 rerun + fake done/failed + prompt-tab-newline + registry + workspace_escape_rejected + invalid_session_rejected + single_workspace_failclosed + pre-run cancel + in-flight cancel + real ui run (session real) + bulk sync (1500 events, $bulk_bytes bytes via stdin) + lease takeover (child stopped) + heartbeat rejection (failed, no done)"
