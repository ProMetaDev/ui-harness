-- Migration 001: append-only event mirror, derived task/approval tables,
-- lease-based claim/heartbeat, and the initial sync function.
--
-- This file is intentionally idempotent: schema.sql may be re-run against an
-- existing database.  It must be safe to apply on top of the pre-versioning
-- schema that existed before ui_schema_migrations was introduced.

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

-- turn/start is the source of truth for tasks; M2 materializes it for leases.
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

-- approval/requested + approval/answered is the source of truth for approvals.
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

-- M2 materialization: tasks/approvals are derived from events, then backed by
-- tables that the lease/state machine can own.
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

-- One task can only be claimed by one worker at a time.
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

-- Renewal: only the current worker can extend its own lease.
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

-- Replay/audit constraint: events cannot be updated or deleted (trigger, not
-- just convention).
create or replace function ui_events_immutable() returns trigger as $$
begin
  raise exception 'ui_events is append-only';
end;
$$ language plpgsql;

drop trigger if exists ui_events_no_update on ui_events;
create trigger ui_events_no_update
before update or delete on ui_events
for each row execute function ui_events_immutable();
