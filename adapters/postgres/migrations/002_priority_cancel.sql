-- Migration 002: task priority + cooperative cancellation.
--
-- Idempotent on purpose: an existing deployment may have 001 applied but have
-- lost the version row, or a future schema.sql rerun may need to converge.
-- Never change a task to done after cancellation was requested.

alter table ui_task_records
  add column if not exists priority int not null default 0;
alter table ui_task_records
  add column if not exists cancel_requested boolean not null default false;

-- Claim order is priority DESC, created_at ASC (task_seq only breaks exact
-- ties so the result is deterministic).
create index if not exists ui_task_records_claim_order
  on ui_task_records (priority desc, created_at asc, task_seq asc)
  where status in ('queued', 'running');

-- Re-derive task status, but never overwrite a cancelled task or a running
-- task whose cancellation has already been requested.
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
    and t.status in ('queued', 'running')
    and not t.cancel_requested;

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

-- Claim the highest-priority queued task, oldest first.  Expired leases remain
-- reclaimable even if cancellation was requested, so a worker can claim the
-- task and complete the cancellation instead of leaving it stuck as running.
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
    where (x.status = 'queued' and not x.cancel_requested)
       or (x.status = 'running' and x.lease_until < now())
    order by x.priority desc, x.created_at asc, x.task_seq asc
    for update skip locked
    limit 1
  )
  returning t.workspace_id, t.session_id, t.task_seq;
end;
$$ language plpgsql;

-- Cancel a task.  A queued task becomes cancelled immediately; a running task
-- keeps its status but gets cancel_requested=true so its worker can stop it.
-- Returns the resulting status, or NULL when the task does not exist / is
-- already terminal.
create or replace function ui_cancel_task(
  p_workspace text, p_session text, p_task_seq bigint
) returns text as $$
declare
  v_status text;
begin
  update ui_task_records
  set cancel_requested = true,
      status = case when status = 'queued' then 'cancelled' else status end,
      finished_at = case when status = 'queued' then now() else finished_at end,
      lease_until = case when status = 'queued' then null else lease_until end
  where workspace_id = p_workspace
    and session_id = p_session
    and task_seq = p_task_seq
    and status in ('queued', 'running')
  returning status into v_status;

  return v_status;
end;
$$ language plpgsql;
