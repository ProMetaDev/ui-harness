# 企业单租户运维 Runbook

> 目标环境的操作手册。M1 之前部分命令是契约占位，**不能当成已实现**；每一节都标注“现状/目标”。
>
> 原则：先保证据，再恢复服务；不删除日志，不跳过审批，不静默降级。

## 1. 服务拓扑

| 组件 | 目标副本 | 作用 |
|---|---:|---|
| gateway | 2+ | OIDC、REST、SSE、限流 |
| control-plane | 2+ | session/task/approval、policy |
| worker | 2+ | agent loop、lease、tool dispatch |
| runner | 按需 | 隔离执行工具 |
| postgres | 主 + 备 | 事件、任务、审批 |
| object store | 多副本 | spill、artifacts、审计包 |
| prometheus/otel | 2 | 指标/trace |
| vault/kms | 托管 | 密钥 |

## 2. 启动前检查

```bash
# 目标命令
curl -fsS https://<host>/healthz
curl -fsS https://<host>/metrics | head
psql "$DATABASE_URL" -c 'select 1'
```

必须确认：

- OIDC discovery 可达；
- DB 迁移版本正确；
- `session_heads` 无空 hash；
- object store 可写；
- runner 池 healthy；
- 告警通道可用。

## 3. 发布与回滚

### 发布

1. 冻结 schema 变更，先跑迁移 dry-run；
2. 备份 DB + object store 清单；
3. 滚动升级 control-plane；
4. 升级 worker，确认 lease 正常；
5. 升级 runner 镜像；
6. 跑 smoke：登录、创建 session、发一条只读任务、审批一次；
7. 观察 30 分钟错误率/队列。

### 回滚

- 应用回滚到上一镜像 tag；
- 数据库只做**向前兼容**迁移；破坏性迁移必须分两次发布；
- 事件不删除；新字段用 nullable/default；
- 回滚后重放/重试任务前先确认没有重复副作用。

## 4. 备份与恢复

### 目标

- RPO ≤ 5 分钟；
- RTO ≤ 1 小时；
- 备份加密；
- 每季度恢复演练。

### 备份

```bash
pg_dump --format=custom --no-owner "$DATABASE_URL" > backup.dump
# object store: 版本化 bucket / inventory
```

### 恢复

1. 停写 control-plane；
2. 恢复 Postgres 到新实例；
3. 校验 `events` hash chain、`session_heads`；
4. 恢复 object store；
5. 切流；
6. 跑 smoke；
7. 记录恢复时间与丢失窗口。

禁止：只恢复 DB 不恢复 object store；只恢复 events 不恢复 tasks/approvals。

## 5. Worker 排空

```text
1. 标记 worker draining（停止领取新 task）
2. 等待当前 task 完成或到达 deadline
3. 释放 lease
4. 确认没有 running task 指向该 worker
5. 下线
```

强制下线前：

- 记录 open tool/turn；
- 让 lease 过期而不是手工改状态；
- 需要重放时走 resume 审批。

## 6. 数据库故障

- 主库不可用：control-plane 进入 fail closed，拒绝新 task；
- 不自动把 running 改成 queued，除非能证明没有副作用；
- 备库提升后，先跑 hash chain + lease 检查；
- 队列恢复后按 `created_at` 重领，限制并发，避免风暴。

## 7. 密钥轮换

- OIDC client secret：双密钥窗口；
- cookie 签名密钥：支持旧 key 验证，新 key 签发；
- runner mTLS：证书滚动，先加新证书再删旧；
- KMS data key：重新包裹，不重写明文；
- 轮换后检查旧 key 使用量为 0。

## 8. 审计导出

```text
GET /v1/audit/export?session=<id>
```

导出包必须包含：

- events（原始字节/规范 JSON）；
- seq/hash/prev；
- 导出时间、操作者、范围；
- 校验和/签名；
- 脱敏声明。

验证：

```text
1. 验 hash chain
2. 验签名
3. 对照 session_heads
4. 记录导出审计事件
```

## 9. 事故响应

### 分级

- SEV1：数据泄露、越权执行、审计链断裂；
- SEV2：大面积不可用、队列堆积、审批不可用；
- SEV3：单 session/单 runner 故障。

### 流程

1. 冻结相关 runner/worker；
2. 保存日志、DB 快照、object store 清单；
3. 确认影响用户/会话/时间窗；
4. 不在原库上“修复证据”；
5. 恢复服务；
6. 24h 内出事故报告；
7. 补回归测试。

## 10. 配额与限流

- 每 workspace：并发 task、每日 tokens、存储；
- 每 runner：CPU/内存/时间/磁盘；
- 每用户：审批频率、消息速率；
- 超限返回明确错误，不静默排队到无限。

## 11. 告警

| 告警 | 阈值 |
|---|---|
| API 5xx | >1% 5min |
| task 等待 | p95 > 30s 10min |
| event append 失败 | >0 连续 3 次 |
| approval 等待 | >5min |
| runner 失败 | >5% 10min |
| DB 连接 | 失败率 >1% |
| 备份 | 超过 24h 未成功 |
| hash verify | 任一失败立即 SEV1 |

## 12. 当前本地版应急

在企业版实现前，本地版仍按：

```bash
bash tools/debug.sh
ui verify --workspace <ws>
ui doctor --workspace <ws>
```

- 不直接编辑 `journal.jsonl`；
- 不手工删 `.ui`；
- 崩溃后先备份 `.ui/`，再 `ui resume`；
- 网络/磁盘异常时停写，不“继续跑看看”。

## 13. 发布检查清单

- [ ] schema 迁移可回滚/向前兼容
- [ ] 备份完成
- [ ] 审计链验证通过
- [ ] OIDC/RBAC smoke
- [ ] 审批 smoke
- [ ] runner 隔离 smoke
- [ ] 24h soak 最近一次通过
- [ ] 回滚镜像可用
- [ ] runbook 更新
