-- Migration 003: workspace lease + JSON claim + worker error recording.
--
-- 二轮评审 N-P1-2 的三条协议/隔离问题：
--   1. claim 只按 tab 分隔字段，prompt 里有 tab/换行就错位 -> 加 JSON claim；
--   2. 两个 worker 可以同时跑同一 workspace 的不同任务 -> 加 workspace lease；
--   3. fail closed 时没有地方记录原因 -> ui_task_records.last_error。
--
-- 幂等：schema.sql 可能被重复执行；本文件也可能在版本行丢失后被重放。
-- 不要在这里改 ui_events 的 append-only 约束。

alter table ui_task_records
  add column if not exists last_error text;

-- 一个 workspace 同时只允许一个 worker 持有写租约。租约与任务租约一样是
-- 60s、由 ui_heartbeat_task 续租；worker 崩溃时两者一起过期，别的 worker
-- 才能接手（不会出现"任务过期了但工作区还被旧 worker 占着"的长期卡死）。
create table if not exists ui_workspace_leases (
  workspace_id text primary key,
  worker_id text not null,
  lease_until timestamptz not null
);

create index if not exists ui_workspace_leases_expiry
  on ui_workspace_leases (lease_until);

-- 领取任务 = 先拿 workspace 租约，再标记任务 running。顺序固定为
-- workspace -> task（heartbeat 也是这个顺序），避免两个事务反向加锁。
--
-- 同一 workspace 里还有别的 queued 任务时，本函数会跳到下一个 workspace，
-- 而不是把任务留给"同一个 worker 的另一个进程"——同一 workspace 的两个
-- worker 同时写 journal 会把 append-only 事件流写乱。
create or replace function ui_claim_task(p_worker text)
returns table (workspace_id text, session_id text, task_seq bigint) as $$
#variable_conflict use_column
declare
  r record;
  skipped text[] := array[]::text[];
begin
  loop
    select x.workspace_id, x.session_id, x.task_seq
      into r
    from ui_task_records x
    where (
        (x.status = 'queued' and not x.cancel_requested)
        or (x.status = 'running' and x.lease_until < now())
      )
      and not (
        (x.workspace_id || chr(1) || x.session_id || chr(1) || x.task_seq::text)
        = any(skipped)
      )
    order by x.priority desc, x.created_at asc, x.task_seq asc
    for update skip locked
    limit 1;

    if not found then
      return;
    end if;

    insert into ui_workspace_leases (workspace_id, worker_id, lease_until)
    values (r.workspace_id, p_worker, now() + interval '60 seconds')
    on conflict (workspace_id) do update
      set worker_id = excluded.worker_id,
          lease_until = excluded.lease_until
      where ui_workspace_leases.worker_id = p_worker
         or ui_workspace_leases.lease_until < now();

    if not found then
      -- 别的 worker 正持有这个 workspace 的写租约；先看下一个候选。
      skipped := array_append(
        skipped,
        r.workspace_id || chr(1) || r.session_id || chr(1) || r.task_seq::text
      );
      continue;
    end if;

    return query
    update ui_task_records t
    set status = 'running',
        worker_id = p_worker,
        lease_until = now() + interval '60 seconds',
        attempts = t.attempts + 1,
        last_error = null
    where t.workspace_id = r.workspace_id
      and t.session_id = r.session_id
      and t.task_seq = r.task_seq
    returning t.workspace_id, t.session_id, t.task_seq;
    return;
  end loop;
end;
$$ language plpgsql;

-- heartbeat 只对"仍持有 workspace 租约"的 worker 返回 true。任务行先不动：
-- 工作区租约被别人拿走时，旧 worker 立即失去续租资格（主循环收到 false 就
-- 终止本地 ui），避免它继续往同一目录写。
create or replace function ui_heartbeat_task(
  p_workspace text, p_session text, p_task_seq bigint, p_worker text
) returns boolean as $$
declare
  n int;
begin
  update ui_workspace_leases
  set lease_until = now() + interval '60 seconds'
  where workspace_id = p_workspace
    and worker_id = p_worker;
  get diagnostics n = row_count;
  if n = 0 then
    return false;
  end if;

  update ui_task_records
  set lease_until = now() + interval '60 seconds'
  where workspace_id = p_workspace
    and session_id = p_session
    and task_seq = p_task_seq
    and worker_id = p_worker
    and status = 'running';
  get diagnostics n = row_count;
  return n > 0;
end;
$$ language plpgsql;

-- 数据库端输出 claim JSON，worker 只 json.loads。prompt 可以是任意文本
-- （tab/换行/引号都不需要转义猜测）。
create or replace function ui_claim_task_json(p_worker text)
returns text as $$
  select coalesce(
    json_agg(
      json_build_object(
        'task_seq', c.task_seq,
        'session', c.session_id,
        'workspace_id', c.workspace_id,
        'prompt', t.prompt,
        'cancel_requested', t.cancel_requested
      )
    ),
    '[]'::json
  )::text
  from ui_claim_task(p_worker) c
  join ui_task_records t
    on t.workspace_id = c.workspace_id
   and t.session_id = c.session_id
   and t.task_seq = c.task_seq;
$$ language sql;
