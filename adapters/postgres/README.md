# Postgres adapter（L2，可选）

> 不属于 `ui-core`；需要 Postgres/`psql`。core 仍然 std-only。

## 1. 能做什么

- `migrate.sh`：幂等迁移入口，只执行 `schema.sql`，可反复运行；
- `schema.sql`：先建 `ui_schema_migrations`，再按版本执行
  `migrations/*.sql`；重复执行不会重复建表或重复插入版本；
- `migrations/001_baseline.sql`：append-only `ui_events`、触发器、
  `ui_tasks` / `ui_approvals` view、任务/审批物化表、claim/heartbeat；
- `migrations/002_priority_cancel.sql`：任务 `priority` 与
  `cancel_requested`，按 `priority DESC, created_at ASC` 领取，以及
  `ui_cancel_task(...)`；
- `export_journal.py`：把 `journal.jsonl` 转成 `COPY` 文本；
- `import.sh`：`schema.sql` + `export_journal.py | psql`；用于导入一份
  journal 快照（`COPY` 本身不是幂等导入，迁移请用 `migrate.sh`）；
- `worker.py`：claim → 运行 `ui run` → heartbeat → done/failed/cancelled。

## 2. 用法

```bash
export DATABASE_URL='postgres://user:pass@127.0.0.1:5432/ui'
bash adapters/postgres/import.sh \
  /path/to/workspace/.ui/journal.jsonl \
  default \
  w1
```

迁移入口可以单独重复执行（不导入 journal）：

```bash
bash adapters/postgres/migrate.sh
```

也可以直接执行 psql 入口：

```bash
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
  -f adapters/postgres/schema.sql
```

`schema.sql` 靠 psql 的 `\ir` 加载同目录 `migrations/`，所以要么从仓库完整
目录执行，要么把整个 `adapters/postgres/` 一起复制到目标机；不要只拷
`schema.sql` 一个文件。

`import.sh` 仍负责“导 schema + 导入一份 journal”，其中 `COPY` 数据是
一次性快照，不承诺同一份 journal 重复导入不报错；需要重复执行的是
`migrate.sh` 的迁移部分。

没有 `psql` 时不应假装导入成功；它是显式的 L2 依赖。

## 3. 任务优先级与取消

`ui_task_records` 新增：

- `priority int not null default 0`；
- `cancel_requested boolean not null default false`。

`ui_claim_task(worker)` 只领 `queued`（或 lease 过期的 `running`）任务，
按 `priority DESC, created_at ASC, task_seq ASC` 排序，用
`FOR UPDATE SKIP LOCKED` 保证同一任务只被一个 worker 领到。

`ui_cancel_task(workspace, session, task_seq)`：

- `queued` → 立即 `cancelled`，并置 `cancel_requested=true`；
- `running` → 保持 `running`，只置 `cancel_requested=true`，由 worker 收尾；
- 已结束或不存在的任务返回 `NULL`，不修改状态。

`worker.py` 的取消处理：

- 每次 claim 前，把自己名下 `running` 且 `cancel_requested` 的任务标成
  `cancelled`（覆盖 worker 重启/两次 claim 之间被取消的情况）；
- claim 到 `cancel_requested=true` 的任务时不启动 `ui`；
- 运行中 heartbeat 同时轮询 `cancel_requested`，命中后终止子进程并写
  `cancelled`；
- 写 `done` 前再次检查 `cancel_requested`，取消与完成竞争时不会把任务
  假装成 `done`。

## 4. 边界

已实现：

- 事件镜像；
- hash/prev 原样保存；
- 任务/审批的只读 view；
- schema 版本表与可重复迁移；
- 单 worker lease + 过期重领；
- 优先级领取与协作式取消；
- 导入失败 fail closed。

未实现：

- Rust core 直连；
- 多 worker 调度器/公平队列（只有数据库行级的 priority + lease）；
- 流式同步（worker 跑完才增量同步）；
- 保留/删除策略；
- 审计签名/WORM。

## 5. 测试

```bash
python3 adapters/postgres/test_export.py
bash tools/verify_postgres_adapter.sh
bash tools/verify_postgres_live.sh
bash tools/verify_postgres_worker.sh
```

`verify_postgres_live.sh` 用 `postgres:16-alpine` 真跑迁移重复执行、journal
导入、priority 顺序、queued cancel、running cancel 请求、lease/heartbeat
和 append-only 触发器；`verify_postgres_worker.sh` 真跑 worker 的 done、
failed、claim 前取消、运行中取消、真实 `ui run` 与 journal 增量同步。
没有 Docker 或镜像时脚本明确 `SKIP`。
