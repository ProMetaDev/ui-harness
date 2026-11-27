# Postgres adapter（L2，可选）

> 不属于 `ui-core`；需要 Postgres/`psql`（worker 还要 Python 3）。core 仍然 std-only。
> 设计说明与完整行为以 [docs/42-postgres-adapter.md](../../docs/42-postgres-adapter.md) 为准。

## 1. 能做什么

- `migrate.sh`：幂等迁移入口，只执行 `schema.sql`，可反复运行；
- `schema.sql`：先建 `ui_schema_migrations`，再按版本号顺序执行
  `migrations/*.sql`；重复执行不会重复建表或重复插入版本；
- `migrations/001_baseline.sql`：append-only `ui_events` + 防改/防删触发器、
  `ui_task_records` / `ui_approval_records` 物化表、`ui_sync_derived()`、
  `ui_claim_task(worker)` / `ui_heartbeat_task(...)`，以及 `ui_tasks` /
  `ui_approvals` 兼容 view；
- `migrations/002_priority_cancel.sql`：任务 `priority` 与
  `cancel_requested`，按 `priority DESC, created_at ASC` 领取，以及
  `ui_cancel_task(...)`；
- `migrations/003_worker_workspace_lease.sql`：任务 `last_error`、
  `ui_workspace_leases(workspace_id, worker_id, lease_until)`（同一 workspace
  同时只有一个 writer），claim 先拿 workspace 租约，heartbeat 同时续任务租约
  与 workspace 租约，`ui_claim_task_json(worker)` 用数据库端 JSON 输出 claim；
- `export_journal.py`：把 `journal.jsonl` 转成 `COPY` 文本；
- `import.sh`：`schema.sql` + `export_journal.py | psql`；用于导入一份
  journal 快照（`COPY` 本身不是幂等导入，迁移请用 `migrate.sh`）；
- `worker.py`：`ui_claim_task_json` → 运行 `ui run` → heartbeat → done/failed/cancelled，
  可选 `--sync-events` 把新事件增量写回 Postgres。

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

### 2.1 启动 worker

多工作区（推荐）：`--workspace-root` 是根目录，claim 里的 `workspace_id`
映射到 `root/<workspace_id>`：

```bash
python3 adapters/postgres/worker.py \
  --dsn "$DATABASE_URL" --worker w1 --ui ./target/release/ui \
  --workspace-root /srv/workspaces --sync-events
```

需要把某个 `workspace_id` 映射到根目录下的多级子目录时，用
`--workspace-registry`（JSON：`{workspace_id: 相对路径}`）：

```bash
cat > /etc/ui/workspace-registry.json <<'EOF'
{"w1": "repos/w1", "team-a": "repos/team-a"}
EOF
python3 adapters/postgres/worker.py \
  --dsn "$DATABASE_URL" --worker w1 --ui ./target/release/ui \
  --workspace-root /srv/workspaces \
  --workspace-registry /etc/ui/workspace-registry.json --sync-events
```

只有一个仓库的旧部署：`--workspace` + `--workspace-id`（ID 默认 `default`），
只服务这一个 `workspace_id`，不是“所有 workspace 都在这个目录里跑”：

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

没有任务时是 `[]`。prompt 里的 tab/换行/引号由数据库端
`json_build_object` / `json_agg` 转义，worker 只 `json.loads`，不再靠分隔符
切分。

## 3. worker 行为

### 3.1 workspace / session 映射

claim 结果里的 `session` 与 `workspace_id` 必须能映射到一个安全目录：

```text
workspace root /srv/ws + workspace_id w1        -> /srv/ws/w1
--workspace-registry {"w1": "repos/w1"}         -> /srv/ws/repos/w1
--workspace /srv/one --workspace-id default     -> /srv/one（只服务 default）
```

- 多工作区模式（`--workspace-root`）：默认映射 `root/<workspace_id>`，
  `workspace_id` 必须是**单个安全目录名**；registry 可映射到 root 下的
  多级子目录；
- 两种模式都拒绝：绝对路径（含 Windows 盘符/UNC）、`..`、`.`、隐藏目录
  分量、NUL，以及 realpath 后跑出 root 的路径（含 symlink 逃逸）；
- registry 文件本身不合法时 worker **启动即失败**，不会等到 claim 后才发现；
- claim 映射不到目录、或 session 不合 `ui` 的命名规则
  （`[A-Za-z0-9_-]`，最长 64）：不启动 `ui`，任务写 `failed`，`last_error`
  记录原因（fail closed，绝不回退到 worker 启动目录）；
- 通过校验后 `mkdir -p` 任务目录；
- `ui run --session`：`default` 是 `<ws>/.ui/journal.jsonl`，其它 session 是
  `<ws>/.ui/sessions/<name>.jsonl`；`--sync-events` 用同一条规则定位本地
  journal，非默认 session 的事件回到同一个 `(workspace_id, session_id)` 分区。

### 3.2 lease 与 `lease_lost`

- 任务租约与 workspace 租约都是 **60s**；worker 每 **2s** heartbeat 一次，
  `ui_heartbeat_task` 同时续期两者；
- heartbeat 发现任务行不再归自己、workspace 租约已被别人拿走、或 psql 调用
  失败：置 `lease_lost` 并记录原因；
- 主循环每 **0.2s** 检查一次，`lease_lost` 优先于正常退出：立即
  `terminate` 本地 `ui` 子进程（5s 未退出则 kill），任务写 `failed`，
  `last_error` 说明 lease 丢失；子进程即使刚好 exit 0 也不会写 `done`；
- 丢租约时**跳过** `--sync-events`：workspace 可能已被别的 worker 接手，
  把本地事件补同步上去会让 `ui_sync_derived()` 按 `turn/end` 改写状态。
  代价是这一段本地 journal 不会自动进库，需要时用 `import.sh` 明确补导；
- 任务进入 done/failed/cancelled 后，worker 释放自己名下不再有 running 任务
  的 workspace 租约；失败时租约 60s 自然过期，不会永久卡住。

### 3.3 增量同步（`--sync-events`）

- 只同步本地 journal 中 `seq >` 数据库已有最大 `seq` 的事件，插入用
  `ON CONFLICT (workspace_id,session_id,seq) DO NOTHING`，重复同步安全；
- 整条 SQL 从 **psql 的 stdin（`-f -`）** 送入，事件再多也不受命令行参数长度
  限制（已验证 1500 条 / 6MB+ 事件）；
- 同步后执行 `ui_sync_derived()`，让任务/审批物化表跟上事件；
- `COPY`（`export_journal.py` / `import.sh`）仍是一次性快照入口，不替代
  worker 的增量同步。

## 4. 任务优先级与取消

`ui_task_records` 新增：

- `priority int not null default 0`；
- `cancel_requested boolean not null default false`。

`ui_claim_task(worker)` 只领 `queued`（或 lease 过期的 `running`）任务，
按 `priority DESC, created_at ASC, task_seq ASC` 排序，用
`FOR UPDATE SKIP LOCKED` 保证同一任务只被一个 worker 领到。`003` 之后它还会
先拿 workspace 租约：同一 workspace 还有别的 queued 任务时跳到下一个
workspace，而不是让两个 worker 同时写同一目录。

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

## 5. 滚动升级要求

- 升级顺序：先 `bash adapters/postgres/migrate.sh`（应用
  `001/002/003`，迁移可重复执行），再启动新版 worker；
- **新旧 worker 混跑期间 workspace 互斥不成立**：workspace 租约只有新版
  worker 才会申请/续期，旧 worker 仍可能领到同一 workspace 的另一个任务。
  迁移要选在没有 worker 运行时，或等旧任务全部跑完再切新版；不能把
  `003` 当成“边跑边滚”的在线迁移；
- `ui_workspace_leases` 不需要回填：迁移后第一个 claim 会创建租约，旧数据
  行不受影响；
- registry 是 worker 本机配置，不进数据库；换机器/换 root 时必须一起带过去，
  否则对应 workspace 的任务 fail closed（`last_error` 写明缺映射）；修好映射
  后需要把任务重新置为 `queued` 才会被重试（没有自动重试）；
- 旧版 worker 仍调用 `ui_claim_task`（`003` 保留并替换了它），不会因为新增
  `ui_claim_task_json` 而启动失败；但混跑期间不要指望新的 workspace 互斥语义。

## 6. 边界

已实现：

- 事件镜像；
- hash/prev 原样保存；
- 任务/审批的只读 view；
- schema 版本表与可重复迁移（001/002/003）；
- 单 worker 任务 lease + workspace 租约 + 过期重领；
- 优先级领取与协作式取消；
- JSON claim 与 psql stdin 增量同步；
- 导入失败 fail closed。

未实现：

- Rust core 直连；
- 多 worker 调度器/公平队列（只有数据库行级的 priority + lease + workspace
  互斥；一个 worker 仍然一次只跑一个任务）；
- 流式同步（worker 跑完才增量同步）；
- 保留/删除策略；
- 审计签名/WORM；
- 真实 Postgres CI（live 脚本已验证，CI 尚未接入）。

已明确不承诺的：

- workspace 租约是协作式的：直接改数据库把 `worker_id`/`status` 改掉，或让
  旧版 worker 参与，互斥就不成立；它防的是正常的多 worker 并发，不是有权限
  的运维误操作；
- 映射失败的任务不会自动重试；需要人工修 registry 后重新入队；
- session 名必须是 `ui` 认识的字符集；数据库里已有非法 session 的任务会按
  失败处理，而不是被悄悄改到 `default`。

## 7. 测试

```bash
python3 adapters/postgres/test_export.py
bash tools/verify_postgres_adapter.sh
bash tools/verify_postgres_live.sh
bash tools/verify_postgres_worker.sh
```

`verify_postgres_live.sh` 用 `postgres:16-alpine` 真跑：schema 迁移重复执行
（版本 001/002/003）、journal 导入、`ui_sync_derived()`、claim/heartbeat/
过期重领、workspace 租约互斥与释放、`ui_claim_task_json` 对含 tab/换行
prompt 的输出、priority 顺序、queued/running cancel、append-only 触发器；
`verify_postgres_worker.sh` 真跑 worker 的 done/failed、registry 映射、
`../escape` 与绝对路径拒绝、非法 session 拒绝、claim 前取消、运行中取消、
真实 `ui run` 写非默认 session journal 并增量同步、1500 条/6MB+ 事件经
psql stdin 同步、被别的 worker 接管后终止本地子进程且不写 done、
`lease_lost` 时写 failed + `last_error`。没有 Docker 或镜像时脚本明确 `SKIP`。
