# Postgres adapter（L2）

> 位置：[adapters/postgres](../adapters/postgres/README.md)
>
> 不属于 `ui-core`；core 继续零依赖。需要 `psql`/Python 时才启用。

## 1. 已实现

- `schema.sql`
  - 幂等迁移入口；先建 `ui_schema_migrations`，再按版本执行
    `migrations/*.sql`；
  - 重复执行不重复建表、不重复插版本，已有数据保留；
  - 依赖 psql 的 `\ir`，需保留 `adapters/postgres/` 目录结构。
- `migrate.sh`
  - 只执行 `schema.sql` 的幂等迁移入口，可反复运行；
  - 与 `import.sh` 分开，避免把“迁移”和“一次性 journal 导入”混在一起。
- `migrations/001_baseline.sql`
  - `ui_events` append-only；
  - 触发器禁止 update/delete；
  - `ui_task_records` / `ui_approval_records` 物化表；
  - `ui_sync_derived()` 从事件同步任务/审批状态；
  - `ui_claim_task(worker)` 用 `FOR UPDATE SKIP LOCKED` 做单 worker lease；
  - `ui_heartbeat_task(...)` 只允许当前 worker 续租，过期可被重领；
  - `ui_tasks` / `ui_approvals` 兼容 view。
- `migrations/002_priority_cancel.sql`
  - `ui_task_records.priority`（int，默认 0）；
  - `ui_task_records.cancel_requested`（bool，默认 false）；
  - `ui_claim_task` 按 `priority DESC, created_at ASC` 领取；
  - `ui_cancel_task(...)`：queued → cancelled；running → 只置
    `cancel_requested=true`；
  - `ui_sync_derived()` 不会把 `cancelled` / 已请求取消的任务改回 `done`。
- `export_journal.py`
  - journal JSONL → Postgres `COPY` 文本；
  - 保留 `seq/prev/hash/ts`；
  - 转义 tab/newline/backslash；
  - 坏行 fail closed。
- `import.sh`
  - `psql` 执行幂等迁移入口；
  - 导出并导入一份 journal 快照（`COPY` 数据本身是一次性导入；
    重复迁移请用 `migrate.sh`，不要靠重复 `import.sh`）；
  - 失败非零。
- `worker.py`
  - `ui_claim_task` → 运行 `ui run` → heartbeat → done/failed/cancelled；
  - claim 前清理自己名下已请求取消的 running 任务；
  - 运行中由 heartbeat 轮询取消请求，终止子进程并写 `cancelled`；
  - 写 `done` 前再次检查取消标志，避免把取消任务假装成完成；
  - `--sync-events` 把新 journal 事件增量写回 Postgres，再
    `ui_sync_derived()`；
  - 只依赖标准库 + psql。
- `test_export.py`
  - 不依赖运行中的 Postgres。

## 2. 用法

```bash
export DATABASE_URL='postgres://user:pass@127.0.0.1:5432/ui'
bash adapters/postgres/import.sh \
  /path/to/.ui/journal.jsonl default w1
```

单独重复执行迁移（不导入 journal）：

```bash
bash adapters/postgres/migrate.sh
```

离线验证：

```bash
bash tools/verify_postgres_adapter.sh
bash tools/verify_postgres_live.sh
bash tools/verify_postgres_worker.sh
```

`verify_postgres_live.sh` 用 `postgres:16-alpine` 容器真跑：
schema 迁移重复执行、journal 导入、`ui_sync_derived()`、
`ui_task_records/ui_approval_records` 查询、claim/heartbeat/过期重领、
`priority DESC, created_at ASC` 顺序、queued cancel、running cancel
请求、append-only 触发器；没有 Docker/镜像时明确 SKIP。

`verify_postgres_worker.sh` 验证 worker 协议：
迁移重复执行、假 `ui` 的 done/failed、claim 前取消（不启动 `ui`）、
运行中取消（终止子进程并写 `cancelled`）、真实 `ui run --mock` 写
journal、增量同步事件、`status=done`。

## 3. 边界

未实现：

- core 直连 Postgres；
- 自动实时同步（worker 跑完才增量同步，不是流式）；
- 多 worker 调度器（已有数据库行级 priority + lease；没有跨队列公平性）；
- 任务/审批的完整状态机（当前只做 queued/running/done/failed/cancelled
  与 pending/allowed/denied）；
- 保留/删除；
- 审计签名/WORM；
- 真实 Postgres CI（本机 live 脚本已验证；CI 尚未接入）。

## 4. 为什么不做进 core

Postgres 客户端/TLS/连接池都需要第三方依赖或自写协议，会破坏 docs/38 的 L0 约束。  
因此它保持为独立 L2 adapter，core 只保证 journal 可导出。
