# Phase 0：企业单租户架构设计

> 状态：设计基线。M1 之前不写“企业版”实现代码。
>
> 目标：把当前本地内核拆成 **API / Control Plane / Worker / Runner / Postgres / Object Store** 六个可独立部署的部分。

## 1. 目标与非目标

### 目标

- 多用户 OIDC 登录；
- RBAC + 逐人审批；
- 事件/任务/审批持久化；
- 多 worker 与任务租约；
- 隔离 runner；
- 可备份、可恢复、可观测；
- 单租户部署，可升级/回滚。

### 非目标

- 多租户计费；
- 公网 SaaS；
- 跨组织共享；
- 插件市场；
- 无限制的宿主 shell。

## 2. 部署拓扑

```text
gateway (stateless)
  ├─ OIDC callback
  ├─ REST / SSE
  └─ RBAC middleware

control-plane (2+ replicas)
  ├─ session/task/approval API
  ├─ policy evaluation
  └─ audit export

worker (2+ replicas)
  ├─ agent loop
  ├─ task lease
  └─ tool dispatch

runner (pool)
  ├─ container per execution
  ├─ workspace snapshot
  └─ no host network by default

postgres
  ├─ users/roles/memberships
  ├─ sessions/events/tasks/approvals
  └─ audit exports

object store
  ├─ spill / artifacts
  └─ encrypted audit bundles

oidc idp · kms/vault · prometheus/otel · backup
```

## 3. 身份与 RBAC

### 角色

| 角色 | 权限 |
|---|---|
| owner | 所有权限；管理组织设置 |
| admin | 用户/角色、策略、workspace、runner |
| operator | 创建会话、发任务、审批自己有权批的调用 |
| auditor | 只读审计、导出、验证；不看正文（可配置） |
| viewer | 只读会话，不可执行、不可审批 |

### 权限点

```text
session:create/read/delete
task:create/cancel
approval:decide
policy:read/write
audit:read/export
secret:use
runner:admin
workspace:admin
```

### 关键不变量

- 每个 task/approval/event 都有 `actor_id`；
- 审批必须同时匹配 `approval_id + call_id + actor`；
- `auditor` 默认只能看脱敏正文与完整性证明；
- 角色变更写审计事件；
- OIDC `sub` 是稳定身份，邮箱不是主键。

## 4. Postgres 数据模型

```sql
create table users (
  id uuid primary key,
  oidc_subject text not null unique,
  email text not null,
  display_name text not null,
  disabled boolean not null default false,
  created_at timestamptz not null default now()
);

create table workspaces (
  id uuid primary key,
  name text not null,
  created_by uuid not null references users(id),
  created_at timestamptz not null default now()
);

create table memberships (
  workspace_id uuid not null references workspaces(id),
  user_id uuid not null references users(id),
  role text not null check (role in ('owner','admin','operator','auditor','viewer')),
  primary key (workspace_id, user_id)
);

create table sessions (
  id uuid primary key,
  workspace_id uuid not null references workspaces(id),
  name text not null,
  created_by uuid not null references users(id),
  created_at timestamptz not null default now(),
  unique (workspace_id, name)
);

-- 每个 session 一行，append 时 SELECT ... FOR UPDATE 串行化 seq/hash
create table session_heads (
  session_id uuid primary key references sessions(id),
  last_seq bigint not null default 0,
  last_hash text not null
);

create table events (
  id uuid primary key,
  workspace_id uuid not null references workspaces(id),
  session_id uuid not null references sessions(id),
  seq bigint not null,
  kind text not null,
  actor_id uuid references users(id),
  turn_id uuid,
  payload jsonb not null,
  prev_hash text not null,
  hash text not null,
  created_at timestamptz not null default now(),
  unique (session_id, seq)
);

create table tasks (
  id uuid primary key,
  session_id uuid not null references sessions(id),
  created_by uuid not null references users(id),
  prompt text not null,
  status text not null check (status in ('queued','running','succeeded','failed','cancelled')),
  attempts int not null default 0,
  max_attempts int not null default 3,
  lease_until timestamptz,
  worker_id text,
  created_at timestamptz not null default now(),
  started_at timestamptz,
  finished_at timestamptz
);

create table approvals (
  id uuid primary key,
  task_id uuid not null references tasks(id),
  call_id text not null,
  requested_by uuid references users(id),
  effects jsonb not null,
  status text not null check (status in ('pending','allowed','denied','expired','cancelled')),
  scope text check (scope in ('once','session')),
  decided_by uuid references users(id),
  reason text,
  expires_at timestamptz not null,
  decided_at timestamptz,
  unique (task_id, call_id)
);

create table artifacts (
  id uuid primary key,
  session_id uuid not null references sessions(id),
  kind text not null,
  object_key text not null,
  sha256 text not null,
  bytes bigint not null,
  created_at timestamptz not null default now()
);
```

### 事件 append 事务

```sql
begin;
select last_seq, last_hash from session_heads where session_id = $1 for update;
-- 计算 seq/hash，插入 events
update session_heads set last_seq = $2, last_hash = $3 where session_id = $1;
commit;
```

- `unique(session_id, seq)` 是第二道保险；
- append 失败不得返回成功；
- 事件只 insert，不 update/delete；
- 审计导出按 `id`/`seq` 稳定排序。

## 5. 任务租约

```sql
update tasks
set status='running', worker_id=$1, lease_until=now()+interval '60 seconds',
    attempts=attempts+1, started_at=now()
where id = (
  select id from tasks
  where status='queued'
     or (status='running' and lease_until < now())
  order by created_at
  for update skip locked
  limit 1
)
returning *;
```

- worker 每 20s 续租；
- 租约过期可被其他 worker 重领；
- 重领超过 `max_attempts` 进入 `failed`，并写 `task/failed`；
- 工具副作用仍由事件恢复协议处理，不能只靠重试。

## 6. Runner 协议

```json
POST /runner/v1/execute
{
  "executionId": "uuid",
  "workspace": {"objectKey": "snapshots/...", "sha256": "..."},
  "tool": "shell",
  "args": {"command": "..."},
  "limits": {
    "cpuMs": 30000,
    "memoryMb": 512,
    "diskMb": 1024,
    "network": "deny"
  },
  "deadline": "2026-11-21T12:00:00Z"
}
```

响应：

```json
{
  "ok": true,
  "content": "...",
  "effects": [{"kind":"fs.write","target":"out.txt"}],
  "artifacts": [{"key":"...","sha256":"...","bytes":123}],
  "sandbox": "container:..."
}
```

- runner 与 control-plane 双向 mTLS；
- runner 只挂载 workspace 的可写副本；
- rootfs 只读；
- 默认无网络，allowlist 域名/端口；
- 超时/配额超限直接 kill 整个容器；
- 返回的 effects 必须由 control-plane 再校验，不信任 runner 自报。

## 7. API v1

```text
GET  /healthz
GET  /metrics
GET  /v1/me

GET  /v1/sessions
POST /v1/sessions
GET  /v1/sessions/{id}/events?since=&limit=
POST /v1/sessions/{id}/messages
POST /v1/tasks/{id}/cancel

GET  /v1/approvals
POST /v1/approvals/{id}/decide

GET  /v1/audit/export?session=
GET  /v1/usage

POST /v1/admin/policies
GET  /v1/admin/users
```

要求：

- OIDC cookie + CSRF；
- `Idempotency-Key` 用于创建 task/message；
- SSE 使用 `Last-Event-ID`，丢失窗口返回 `events-dropped`；
- 所有响应带 `X-Request-Id`；
- 错误不泄露内部路径/密钥。

## 8. SLO

| 指标 | 目标 |
|---|---|
| API 可用性 | 99.9% |
| task 入队到开始 p95 | < 2s |
| event append p95 | < 100ms |
| 审批等待 p95 | < 60s |
| 工具执行失败可解释率 | 100%（有 error class） |
| 已提交事件丢失 | 0 |
| RPO | ≤ 5 分钟 |
| RTO | ≤ 1 小时 |

## 9. 威胁模型

| 威胁 | 控制 |
|---|---|
| 伪造用户 | OIDC 签名校验、cookie HttpOnly/SameSite、CSRF |
| 越权审批 | approval/call/actor 三重匹配、RBAC |
| 跨会话读取 | workspace_id/session_id 过滤、tool cache session |
| 日志篡改 | append-only、hash chain、签名导出、DB 权限 |
| runner 逃逸 | 容器、只读 rootfs、无网络、配额、seccomp |
| 插件后门 | 显式 allowlist、effects 强制、签名/哈希 |
| 密钥外泄 | Vault/KMS、env scrub、输出脱敏 |
| 重放 | Idempotency-Key、task/approval 唯一键 |
| DoS | 限流、队列上限、配额、超时 |
| 供应链 | SBOM、签名 release、CVE 扫描 |

## 10. 从本地 JSONL 迁移

1. 每个 session 先跑 `stream_verify`；
2. 导入时保留 `seq/prev/hash`，写入 `events`；
3. `session_heads` 取最后 seq/hash；
4. 导入后跑一次全链验证；
5. 失败则整 session 回滚，不半导入；
6. 保留 JSONL 导出能力，便于回滚。

## 11. 验收测试

- 两个用户、五种角色；
- viewer 不能发任务/审批；
- operator 只能审批自己 workspace；
- auditor 能导出并验证，但看不到正文（配置）；
- 杀 worker 后 task 被重领，副作用不重复；
- DB 连接失败 fail closed；
- 事件 append 并发无重复 seq；
- 备份恢复后 seq/hash 完整；
- runner 写宿主失败、网络拒绝、超限被杀；
- 24h soak 无泄漏。

## 12. 待定 ADR

- Postgres vs Kafka 的吞吐阈值；
- 容器 vs microVM 的风险分层；
- 审计正文是否对 auditor 默认可见；
- Windows runner 是否首发；
- 对象存储选型（S3/MinIO/本地）。
