-- Postgres adapter schema entrypoint.
--
-- Run this file with psql, not with a generic SQL client:
--
--   psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f adapters/postgres/schema.sql
--
-- It is the idempotent migration entrypoint.  ui_schema_migrations records
-- which numbered migrations have been applied; the \if blocks below skip them
-- on every later run.  New migrations are added as migrations/NNN_*.sql and
-- appended to this file.  import.sh intentionally calls this file every time,
-- so importing the same journal twice must remain a no-op at the schema level.

\set ON_ERROR_STOP on

begin;

create table if not exists ui_schema_migrations (
  version text primary key,
  description text not null,
  applied_at timestamptz not null default now()
);

select case
         when exists (select 1 from ui_schema_migrations where version = '001_baseline')
         then 'false' else 'true'
       end as apply_001 \gset
\if :apply_001
\ir migrations/001_baseline.sql
insert into ui_schema_migrations (version, description)
values ('001_baseline', 'events, views, task/approval tables, claim/heartbeat')
on conflict (version) do nothing;
\endif

select case
         when exists (select 1 from ui_schema_migrations where version = '002_priority_cancel')
         then 'false' else 'true'
       end as apply_002 \gset
\if :apply_002
\ir migrations/002_priority_cancel.sql
insert into ui_schema_migrations (version, description)
values ('002_priority_cancel', 'task priority ordering and cooperative cancellation')
on conflict (version) do nothing;
\endif

select case
         when exists (select 1 from ui_schema_migrations where version = '003_worker_workspace_lease')
         then 'false' else 'true'
       end as apply_003 \gset
\if :apply_003
\ir migrations/003_worker_workspace_lease.sql
insert into ui_schema_migrations (version, description)
values ('003_worker_workspace_lease', 'workspace lease, JSON claim, worker error recording')
on conflict (version) do nothing;
\endif

commit;
