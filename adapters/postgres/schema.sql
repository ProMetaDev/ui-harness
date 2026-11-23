-- 企业单租户 Postgres 事件镜像（L2 adapter，不属于 ui-core）。
-- 目标：先把 journal 变成可查询、可备份的 append-only 表；
-- 任务/审批先用 view 从事件派生，M2 再物化成表。

create table if not exists ui_events (
  id bigserial primary key,
  workspace_id text not null default 'default',
  session_id text not null,
  seq bigint not null,
  kind text not null,
  actor text,
  turn bigint,
  payload jsonb not null,
  prev_hash text not null,
  hash text not null,
  created_at timestamptz not null,
  unique (workspace_id, session_id, seq)
);

create index if not exists ui_events_session_seq on ui_events (session_id, seq);
create index if not exists ui_events_kind on ui_events (kind);

-- turn/start 是任务的事实来源；M2 再用 lease/status 表接管。
create or replace view ui_tasks as
select
  workspace_id,
  session_id,
  seq as task_seq,
  (payload->>'turn')::bigint as turn,
  payload->>'prompt' as prompt,
  created_at
from ui_events
where kind = 'turn/start';

-- approval/requested + approval/answered 是审批的事实来源。
create or replace view ui_approvals as
select
  workspace_id,
  session_id,
  coalesce(payload->>'approvalId', payload->>'approval_id') as approval_id,
  coalesce(payload->>'callId', payload->>'call_id') as call_id,
  payload->>'tool' as tool,
  payload->'effects' as effects,
  payload->>'verdict' as verdict,
  payload->>'scope' as scope,
  payload->>'via' as via,
  kind,
  created_at
from ui_events
where kind in ('approval/requested', 'approval/answered');

-- M2 物化：任务/审批从事件派生，但落成可被 lease/状态机接管的表。
create table if not exists ui_task_records (
  workspace_id text not null,
  session_id text not null,
  task_seq bigint not null,
  turn bigint,
  prompt text,
  status text not null default 'queued',
  worker_id text,
  lease_until timestamptz,
  attempts int not null default 0,
  created_at timestamptz not null,
  finished_at timestamptz,
  primary key (workspace_id, session_id, task_seq)
);

alter table ui_task_records add column if not exists worker_id text;
alter table ui_task_records add column if not exists lease_until timestamptz;
alter table ui_task_records add column if not exists attempts int not null default 0;

create table if not exists ui_approval_records (
  workspace_id text not null,
  session_id text not null,
  approval_id text not null,
  call_id text,
  tool text,
  effects jsonb,
  status text not null default 'pending',
  scope text,
  via text,
  created_at timestamptz not null,
  decided_at timestamptz,
  primary key (workspace_id, session_id, approval_id)
);

create or replace function ui_sync_derived() returns void as $$
begin
  insert into ui_task_records
    (workspace_id, session_id, task_seq, turn, prompt, status, created_at)
  select workspace_id, session_id, seq, (payload->>'turn')::bigint,
         payload->>'prompt', 'queued', created_at
  from ui_events where kind = 'turn/start'
  on conflict (workspace_id, session_id, task_seq) do nothing;

  update ui_task_records t
  set status = 'done', finished_at = e.created_at
  from ui_events e
  where e.kind = 'turn/end'
    and t.workspace_id = e.workspace_id
    and t.session_id = e.session_id
    and t.turn = (e.payload->>'turn')::bigint
    and t.status <> 'done';

  insert into ui_approval_records
    (workspace_id, session_id, approval_id, call_id, tool, effects, status, created_at)
  select workspace_id, session_id,
         coalesce(payload->>'approvalId', payload->>'approval_id'),
         coalesce(payload->>'callId', payload->>'call_id'),
         payload->>'tool', payload->'effects', 'pending', created_at
  from ui_events where kind = 'approval/requested'
  on conflict (workspace_id, session_id, approval_id) do nothing;

  update ui_approval_records a
  set status = case
        when e.payload->>'verdict' in ('allow','once','allow-once','y','yes') then 'allowed'
        when e.payload->>'verdict' in ('always','session','allow-always','a') then 'allowed'
        else 'denied'
      end,
      scope = e.payload->>'scope',
      via = e.payload->>'via',
      decided_at = e.created_at
  from ui_events e
  where e.kind = 'approval/answered'
    and a.workspace_id = e.workspace_id
    and a.session_id = e.session_id
    and a.approval_id = coalesce(e.payload->>'approvalId', e.payload->>'approval_id');
end;
$$ language plpgsql;

-- M2 lease 基础：一个任务同一时刻只能被一个 worker 领到。
create or replace function ui_claim_task(p_worker text)
returns table (workspace_id text, session_id text, task_seq bigint) as $$
begin
  return query
  update ui_task_records t
  set status = 'running',
      worker_id = p_worker,
      lease_until = now() + interval '60 seconds',
      attempts = t.attempts + 1
  where (t.workspace_id, t.session_id, t.task_seq) = (
    select x.workspace_id, x.session_id, x.task_seq
    from ui_task_records x
    where x.status = 'queued'
       or (x.status = 'running' and x.lease_until < now())
    order by x.created_at
    for update skip locked
    limit 1
  )
  returning t.workspace_id, t.session_id, t.task_seq;
end;
$$ language plpgsql;

-- 续租：只有当前 worker 能延长自己的 lease。
create or replace function ui_heartbeat_task(
  p_workspace text, p_session text, p_task_seq bigint, p_worker text
) returns boolean as $$
declare
  n int;
begin
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

-- 审计/回放约束：事件不可 update/delete（用触发器而不是只靠约定）。
create or replace function ui_events_immutable() returns trigger as $$
begin
  raise exception 'ui_events is append-only';
end;
$$ language plpgsql;

drop trigger if exists ui_events_no_update on ui_events;
create trigger ui_events_no_update
before update or delete on ui_events
for each row execute function ui_events_immutable();
