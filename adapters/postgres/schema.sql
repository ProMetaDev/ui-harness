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
