# 企业级路线图：单租户 Agent 平台

> 目标不是把当前 CLI 包装成“企业版”，而是把内核的**可信边界**升级成组织可运营的服务。
>
> 先做企业单租户，不先做多租户 SaaS。多租户会放大所有身份、存储和执行隔离问题。

## 1. 当前基线

已经具备：

- 可审计的工具策略、审批和副作用日志；
- append-only journal、hash chain、resume、export、fuzz；
- shell/plugin/MCP/grep 的 supervisor、Landlock/Job、env scrub；
- 会话隔离、journal replay、SSE 窗口；
- 本地 CLI、Web、skill、工作流；
- 55 项本地门禁与真实 Windows 进程树验证。

不具备：

- OIDC/SSO、RBAC、逐人审批归属；
- 集中式事件存储和任务队列；
- 多 worker、租约、HA、备份恢复；
- 容器化 runner、租户级配额、网络 egress 策略；
- KMS/Vault、审计 WORM/签名、数据保留与删除；
- 指标、trace、SLO、告警、管理台；
- 企业发布、迁移、回滚和合规证据。

## 2. 目标定义

### Enterprise Single-Tenant v1

一个组织内部部署，满足：

- 多用户通过 OIDC 登录；
- 角色至少 `owner/admin/operator/auditor/viewer`；
- 每次工具/审批都归属到具体用户；
- 事件、任务、审批持久化到 Postgres；
- 至少两个 worker，可滚动升级；
- 工具在隔离 runner 中执行，默认无网络；
- 备份可恢复，RPO ≤ 5 分钟，RTO ≤ 1 小时；
- 指标、trace、审计导出、告警可用；
- 有升级/回滚/密钥轮换 runbook；
- 通过内部安全评审和 24h soak。

### 独立性约束

本路线不得破坏 [docs/38-independence.md](38-independence.md)：

- `ui-core` / `ui-cli` 保持 Rust std-only；
- Cursor/VS Code/云端/npm/CDN 不作为必需依赖；
- Postgres/OIDC/容器 runner 只能作为可选 adapter；
- 如果要求“全产品零外部程序”，M1/M2/M4 必须重新定义或放弃。

### 明确不做

- 多租户计费；
- 跨组织共享 workspace；
- 公网 SaaS；
- 插件市场；
- 无限制的 shell 直连宿主机。

## 3. 目标架构

```text
                    ┌──────────────┐
   Browser/CLI ───▶ │ API Gateway  │  OIDC / RBAC / rate limit
                    └──────┬───────┘
                           │
                    ┌──────▼───────┐
                    │ Control Plane│  sessions / policies / approvals
                    └───┬──────┬───┘
                        │      │
             ┌──────────▼─┐  ┌─▼────────────┐
             │ Postgres   │  │ Object Store │  spill / artifacts / audit
             │ events     │  └──────────────┘
             │ tasks      │
             │ approvals  │
             └──────┬─────┘
                    │ lease / queue
          ┌─────────▼─────────┐
          │ Worker Pool       │  agent loop, no host shell
          └───────┬───────────┘
                  │ tool request
          ┌───────▼───────────┐
          │ Runner Service    │  container / microVM, CPU/mem/egress quota
          └───────┬───────────┘
                  │
          ┌───────▼───────────┐
          │ Workspace Volume  │  per-session snapshot
          └───────────────────┘

  OIDC IdP     KMS/Vault     Prometheus/OTel     Backup/DR
```

## 4. 工作流

### W1 身份与权限

- OIDC 登录、会话 cookie；
- 角色/权限矩阵；
- 审批人、审计员可归属；
- API token / service account；
- 后续 SCIM。

### W2 事件与任务存储

- Postgres append-only events；
- 每 session 的 seq/hash chain；
- task lease / retry / dead-letter；
- approval 持久化；
- 从本地 JSONL 导入/导出。

### W3 执行隔离

- runner 协议：tool request/response；
- 容器化 shell/plugin/MCP/grep；
- CPU/内存/时间/磁盘配额；
- 默认 deny egress，allowlist；
- Windows runner 可选。

### W4 密钥与数据

- KMS/Vault 托管；
- 静态加密；
- PII 分级、保留、删除；
- 审计 WORM/签名；
- 备份加密。

### W5 可观测性

- metrics：turn、queue、approval、tool、cost；
- traceId 贯穿；
- SLO 仪表盘；
- 告警与 runbook。

### W6 交付与运维

- Docker/Helm/Terraform；
- 迁移工具；
- 滚动升级/回滚；
- 备份/恢复演练；
- SBOM/签名/CVE。

### W7 产品面

- 管理台：用户/角色、策略、审批、审计、用量；
- 前端拆分为 shell/chat/sessions/tools/admin；
- 可访问性与 i18n。

## 5. 里程碑

| 里程碑 | 周期 | 交付 | 退出条件 |
|---|---|---|---|
| M0 设计基线 | 1–2 周 | 本路线图、Phase 0 设计、runbook、readiness 检查 | 评审通过，ADR 定稿 |
| M1 身份 | 2–3 周 | OIDC + RBAC + 逐人审批 | 两个用户不同角色 e2e |
| M2 持久化 | 3–4 周 | Postgres events/tasks/approvals | 杀 worker 不丢任务，不重复副作用 |
| M3 多 worker | 2–3 周 | lease、重试、滚动升级 | 2 worker 并发，升级不中断 |
| M4 runner | 3–4 周 | 容器执行 + 配额 + egress | 越界写入/网络被拒，资源超限被停 |
| M5 运维 | 2–3 周 | metrics/trace/SLO/备份/恢复 | RPO/RTO 演练通过 |
| M6 试点 | 2–4 周 | 管理台、审计导出、安全评审 | 24h soak + 试点团队验收 |

## 6. 90 天计划

### 第 1–30 天

- M0 设计；
- Postgres schema + event/task 抽象；
- OIDC/RBAC；
- Docker Compose 开发环境；
- metrics/trace 基础。

### 第 31–60 天

- 多 worker + lease；
- runner 容器化；
- 审批持久化；
- 备份/恢复；
- 管理台第一版。

### 第 61–90 天

- 安全评审/渗透测试；
- 24h soak；
- DR 演练；
- 发布/回滚；
- 试点验收。

## 7. 验收矩阵

| 能力 | 验收 |
|---|---|
| 身份 | 未登录 401；viewer 不能审批；operator 能跑；auditor 能导出审计 |
| 事件 | 每 session seq 连续；hash 可验；重复提交幂等 |
| 任务 | worker 崩溃后 lease 过期被重领；不重复副作用 |
| 审批 | 审批绑定 user/approvalId/callId；超时/取消可解释 |
| 隔离 | runner 不能写宿主；默认无网络；CPU/mem 超限被杀 |
| 数据 | 备份恢复后事件/任务/审批完整；删除请求可执行 |
| 运维 | 滚动升级无中断；回滚可执行；告警可触发 |
| 审计 | 导出可独立验证；审计员看不到正文但能验完整性 |
| SLO | API 99.9%；turn 启动 p95 < 2s；事件 append p95 < 100ms |
| 负载 | 100 并发任务；24h 无泄漏、无丢事件 |

## 8. 关键决策（ADR）

1. **事件主存储用 Postgres，不用本地 JSONL**；JSONL 保留为导入/导出格式。
2. **任务队列先用 Postgres `FOR UPDATE SKIP LOCKED`**，不引入 Kafka，直到吞吐证明需要。
3. **runner 先容器，不直接上 microVM**；高风险租户再升级。
4. **单租户先做 tenant_id 字段，但不做跨租户路由**；避免以后迁移。
5. **审计事件与执行事件同源**；不允许“审计日志另写一份”。
6. **Windows runner 作为可选执行池**，不阻塞 Linux 首发。

## 9. 风险

- **范围蔓延**：管理台/多租户会吞掉所有时间；先锁定 M1–M4。
- **事件迁移**：本地 journal 导入必须可验证，不能丢 seq/hash。
- **审批语义**：多用户后，“谁批准了什么”必须不可伪造。
- **runner 兼容**：现有 shell/plugin/MCP 的 effects 需要统一协议。
- **成本**：模型费用必须按用户/会话归属。
- **合规**：日志含提示词/工具输出，保留与删除策略必须先定。

## 10. 角色

| 角色 | 责任 |
|---|---|
| Tech Lead | 架构、ADR、里程碑 |
| Backend | 事件存储、任务、worker、runner |
| Platform/SRE | 部署、HA、备份、观测、SLO |
| Security | 威胁模型、渗透、密钥、审计 |
| Frontend | 管理台、聊天、审批、可访问性 |
| Product/Ops | 试点、流程、培训、验收 |

## 11. 下一步

先完成 [Phase 0 设计](36-enterprise-phase0.md) 和 [运维 runbook](37-enterprise-runbook.md)，再用 `tools/verify_enterprise_plan.sh` 做设计门禁；通过后才进入 M1 编码。
