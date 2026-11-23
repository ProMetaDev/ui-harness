# Postgres adapter（L2）

> 位置：[adapters/postgres](adapters/postgres/README.md)
>
> 不属于 `ui-core`；core 继续零依赖。需要 `psql`/Python 时才启用。

## 1. 已实现

- `schema.sql`
  - `ui_events` append-only；
  - 触发器禁止 update/delete；
  - `ui_task_records` / `ui_approval_records` 物化表；
  - `ui_sync_derived()` 从事件同步任务/审批状态；
  - `ui_tasks` / `ui_approvals` 兼容 view。
- `export_journal.py`
  - journal JSONL → Postgres `COPY` 文本；
  - 保留 `seq/prev/hash/ts`；
  - 转义 tab/newline/backslash；
  - 坏行 fail closed。
- `import.sh`
  - `psql` 建 schema；
  - 导出并导入；
  - 失败非零。
- `test_export.py`
  - 不依赖运行中的 Postgres。

## 2. 用法

```bash
export DATABASE_URL='postgres://user:pass@127.0.0.1:5432/ui'
bash adapters/postgres/import.sh \
  /path/to/.ui/journal.jsonl default w1
```

离线验证：

```bash
bash tools/verify_postgres_adapter.sh
bash tools/verify_postgres_live.sh
```

`verify_postgres_live.sh` 用 `postgres:16-alpine` 容器真跑 schema + COPY +
`ui_sync_derived()` + `ui_task_records/ui_approval_records` 查询 + append-only 触发器；
没有 docker/镜像时明确 SKIP。

## 3. 边界

未实现：

- core 直连 Postgres；
- 自动增量同步；
- 多 worker lease；
- 任务/审批的完整状态机（当前只做 queued/done 与 pending/allowed/denied）；
- 保留/删除；
- 审计签名/WORM；
- 真实 Postgres CI（本机 live 脚本已验证；CI 尚未接入）。

## 4. 为什么不做进 core

Postgres 客户端/TLS/连接池都需要第三方依赖或自写协议，会破坏 docs/38 的 L0 约束。  
因此它保持为独立 L2 adapter，core 只保证 journal 可导出。
