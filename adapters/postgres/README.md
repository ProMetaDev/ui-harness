# Postgres adapter（L2，可选）

> 不属于 `ui-core`；需要 Postgres/`psql`。core 仍然 std-only。

## 1. 能做什么

- `schema.sql`：append-only `ui_events`，触发器禁止 update/delete；
- `ui_tasks` / `ui_approvals`：先从事件派生的 view；
- `export_journal.py`：把 `journal.jsonl` 转成 `COPY` 文本；
- `import.sh`：`schema.sql` + `export_journal.py | psql`。

## 2. 用法

```bash
export DATABASE_URL='postgres://user:pass@127.0.0.1:5432/ui'
bash adapters/postgres/import.sh \
  /path/to/workspace/.ui/journal.jsonl \
  default \
  w1
```

没有 `psql` 时不应假装导入成功；它是显式的 L2 依赖。

## 3. 边界

已实现：

- 事件镜像；
- hash/prev 原样保存；
- 任务/审批的只读 view；
- 导入失败 fail closed。

未实现：

- Rust core 直连；
- 多 worker lease；
- 任务/审批物化与状态机；
- 自动增量同步；
- 删除/保留策略；
- 审计签名/WORM。

## 4. 测试

```bash
python3 adapters/postgres/test_export.py
```

只测导出文本，不依赖运行中的 Postgres。真正导入需要一个测试库。
