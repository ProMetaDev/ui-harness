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
- `migrations/003_worker_workspace_lease.sql`
  - `ui_task_records.last_error`：worker 拒绝任务时把原因落库（不再只有
    stderr）；
  - `ui_workspace_leases(workspace_id, worker_id, lease_until)`：
    **一个 workspace 同时只允许一个 worker 持有写租约**；lease 60s；
  - `ui_claim_task(worker)` 改成先拿 workspace 租约再标记任务 running。
    同一 workspace 还有别的 queued 任务时跳到下一个 workspace，而不是
    让两个 worker 同时写同一目录；
  - `ui_heartbeat_task(...)` 同时续 workspace 租约；任务不归自己、或工作区
    租约已被别人拿走时返回 false（worker 据此终止本地 `ui`）；
  - `ui_claim_task_json(worker)`：数据库端 `json_build_object` +
    `json_agg` 输出 claim JSON。prompt 里的 tab/换行/引号不再需要靠分隔符
    猜。
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
  - `ui_claim_task_json` → 运行 `ui run --workspace <任务目录> --session
    <claim 里的 session>` → heartbeat → done/failed/cancelled；
  - claim 前清理自己名下已请求取消的 running 任务；
  - 运行中由 heartbeat 轮询取消请求，终止子进程并写 `cancelled`；
  - 写 `done` 前再次检查取消标志，避免把取消任务假装成完成；
  - `--sync-events` 把新 journal 事件增量写回 Postgres，再
    `ui_sync_derived()`；SQL 从 psql 的 stdin（`-f -`）送入，事件再多也不
    受命令行参数长度限制；
  - heartbeat 丢租约（返回 false 或 psql 报错）时终止本地子进程、写
    `failed` 并记 `last_error`，**不写 done**；丢租约时也不同步事件，避免
    替已经接手的工作区写状态；
  - 只依赖标准库 + psql。
- `test_export.py`
  - 不依赖运行中的 Postgres。

### 1.1 会话与工作区映射（N-P1-2）

claim 结果里包含 `session` 与 `workspace_id`（旧实现查出来却只用固定的
`self.workspace`，导致非默认会话的事件同步到错的地方）。worker 现在按
workspace_id 映射目录，并把 session 原样传给 `ui`：

```text
workspace root /srv/ws + workspace_id w1        -> /srv/ws/w1
--workspace-registry {"w1": "repos/w1"}         -> /srv/ws/repos/w1
--workspace /srv/one --workspace-id default     -> /srv/one（只服务 default）
```

规则与失败方式：

- 多工作区模式（`--workspace-root`）：默认映射是
  `root/<workspace_id>`，要求 workspace_id 是**单个安全目录名**；
  `--workspace-registry` 的 JSON（`{workspace_id: 相对路径}`）可以把某个
  工作区映射到 root 下的子目录（可多级）。
- 单工作区模式（`--workspace DIR --workspace-id ID`，ID 默认 `default`）：
  只服务一个 workspace_id。这是给旧部署/只有一个仓库的 worker 用的明确
  映射，不是“所有 workspace 都在这个目录里跑”。
- 两种模式都拒绝：绝对路径（含 Windows 盘符/UNC）、`..`、`.`、隐藏目录
  分量、NUL，以及 realpath 之后跑出 root 的路径（含 symlink 逃逸）。
- registry 文件本身不合法（不是对象、值是绝对路径/`..`）时 worker **启动
  即失败**，不会等到 claim 后才发现。
- claim 到的任务映射不到目录、或 session 不合 `ui` 的命名规则
  （`[A-Za-z0-9_-]`，最长 64）时：不启动 `ui`，任务直接写 `failed`，
  `last_error` 记录原因。这是 fail closed——回退到 worker 启动目录里跑会
  把 A 工作区的事件写进 B 的 journal。
- 通过校验后 worker 会 `mkdir -p` 任务目录（新工作区首次运行时需要）。

`ui run --session` 的行为：`default` 仍是 `<ws>/.ui/journal.jsonl`；其它
session 是 `<ws>/.ui/sessions/<name>.jsonl`。同步路径与 `ui` 的
`resolve_journal` 保持一致，所以非默认会话的事件能回到同一个
`(workspace_id, session_id)` 分区。

### 1.2 lease_lost 与工作区互斥

- 任务租约与 workspace 租约都是 60s，`ui_heartbeat_task` 同时续期；
  worker 2s heartbeat 一次。
- heartbeat 线程发现 `ui_heartbeat_task` 返回 false、任务行不再归自己、
  或 psql 调用失败，会置 `lease_lost` 并记录原因。
- 主循环每 0.2s 检查一次：`lease_lost` 优先于正常退出，发现后立即
  `terminate` 本地 `ui` 子进程（5s 未退出则 kill），任务写 `failed`，
  `last_error` 说明是 lease 丢失；即使子进程刚好 exit 0 也不会写 `done`。
- 丢租约时跳过 `--sync-events`：工作区可能已被别的 worker 接手，把本地
  事件补同步上去会让 `ui_sync_derived()` 根据 `turn/end` 改写状态。代价是
  这一段的本地 journal 事件不会自动进库，需要时用 `import.sh` 明确补导。
- 任务进入 done/failed/cancelled 后 worker 释放自己名下不再有 running 任务
  的 workspace 租约；别的 worker 才能领同一 workspace 的下一个任务。
- 两条路径都失败时租约 60s 自然过期，不会永久卡住。

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

启动 worker（多工作区，推荐）：

```bash
python3 adapters/postgres/worker.py \
  --dsn "$DATABASE_URL" --worker w1 --ui ./target/release/ui \
  --workspace-root /srv/workspaces --sync-events
```

需要把 workspace_id 映射到 root 下的多级目录时，写 registry：

```bash
cat > /etc/ui/workspace-registry.json <<'EOF'
{"w1": "repos/w1", "team-a": "repos/team-a"}
EOF
python3 adapters/postgres/worker.py \
  --dsn "$DATABASE_URL" --worker w1 --ui ./target/release/ui \
  --workspace-root /srv/workspaces \
  --workspace-registry /etc/ui/workspace-registry.json --sync-events
```

只有一个仓库的旧部署：

```bash
python3 adapters/postgres/worker.py \
  --dsn "$DATABASE_URL" --worker w1 --ui ./target/release/ui \
  --workspace /srv/workspace --workspace-id default --sync-events
```

`--` 之后的参数原样传给 `ui run`（例如 `--mock scenario.json`），但
`--workspace/--session/--journal/--task/--task-file` 由 worker 从 claim 结果
生成，extra 里出现会直接报错退出——否则重复选项会覆盖映射。

claim JSON 形状（`ui_claim_task_json`）：

```json
[{"task_seq":1,"session":"default","workspace_id":"w1",
  "prompt":"line-1\tindent\nline-2","cancel_requested":false}]
```

没有任务时是 `[]`。

### 2.1 迁移/升级边界

- 升级步骤：先跑 `bash adapters/postgres/migrate.sh`（新增
  `003_worker_workspace_lease`），再启动新版 worker。迁移本身可重复执行。
- **滚动升级注意**：workspace 租约只有新版 worker 才会申请/续期。数据库
  迁移完成后、旧 worker 还在跑的时候，新 worker 可能领到同一 workspace
  的另一个任务。建议迁移时机选在没有 worker 运行时，或等旧任务全部跑完
  再切新版；否则这段时间的“同一 workspace 只有一个 writer”不成立。
- `ui_workspace_leases` 不需要回填：迁移后第一个 claim 会创建租约；
  旧数据行不影响。
- registry 是 worker 本机配置，不进数据库；换机器/换 root 时必须一起带过
  去，否则对应 workspace 的任务会 fail closed（`last_error` 写明缺映射）。
  修好映射后需要把该任务重新置为 `queued` 才会被重试（没有自动重试）。

离线/真库验证：

```bash
bash tools/verify_postgres_adapter.sh
bash tools/verify_postgres_live.sh
bash tools/verify_postgres_worker.sh
```

`verify_postgres_live.sh` 用 `postgres:16-alpine` 容器真跑：
schema 迁移重复执行（版本 001/002/003）、journal 导入、
`ui_sync_derived()`、`ui_task_records/ui_approval_records` 查询、
claim/heartbeat/过期重领、workspace 租约互斥与释放、heartbeat 续 workspace
租约、`ui_claim_task_json` 对含 tab/换行 prompt 的输出、
`priority DESC, created_at ASC` 顺序、queued cancel、running cancel
请求、append-only 触发器；没有 Docker/镜像时明确 SKIP。

`verify_postgres_worker.sh` 验证 worker 协议（真 Postgres + 假 `ui`）：
迁移重复执行、假 `ui` 的 done/failed、prompt 含 tab/换行时 argv 原样、
workspace registry 映射、`../escape` 与绝对路径 workspace_id 被拒绝且不
启动 `ui`、非法 session 被拒绝、单工作区模式缺映射 fail closed、claim 前
取消（不启动 `ui`）、运行中取消（终止子进程并写 `cancelled`）、真实
`ui run --mock` 写非默认 session journal 并增量同步、1500 条/6MB+ 事件
经 psql stdin 同步、任务被别的 worker 接管后终止本地子进程且不写 done、
heartbeat 返回 false 时写 failed + `last_error`。

## 3. 边界

未实现：

- core 直连 Postgres；
- 自动实时同步（worker 跑完才增量同步，不是流式）；
- 多 worker 调度器（已有数据库行级 priority + lease + workspace 互斥；
  没有跨队列公平性，一个 worker 仍然一次只跑一个任务）；
- 任务/审批的完整状态机（当前只做 queued/running/done/failed/cancelled
  与 pending/allowed/denied）；
- 保留/删除；
- 审计签名/WORM；
- 真实 Postgres CI（本机 live 脚本已验证；CI 尚未接入）。

已明确不承诺的：

- workspace 租约是协作式的：直接改数据库把 `worker_id`/`status` 改掉，
  或让旧版 worker 参与，互斥就不成立。它防的是正常的多 worker 并发，不是
  有权限的运维误操作。
- 映射失败的任务不会自动重试；需要人工修 registry 后重新入队。
- session 名必须是 `ui` 认识的字符集；数据库里已有非法 session 的任务会
  按失败处理，而不是被悄悄改到 `default`。

## 4. 为什么不做进 core

Postgres 客户端/TLS/连接池都需要第三方依赖或自写协议，会破坏 docs/38 的 L0 约束。  
因此它保持为独立 L2 adapter，core 只保证 journal 可导出。
