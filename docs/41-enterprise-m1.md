# 企业 M1：身份与 RBAC 的零依赖边界

> 状态：边界设计 + 部署契约，尚未实现。
>
> 结论：**OIDC/TLS/JWT 不进入 `ui-core`。** 核心只接受来自受信代理的已认证身份；
> 企业 adapter 负责 OIDC、cookie、组映射。这样不破坏 docs/38 的 L0 零依赖。

## 1. 为什么不能把 OIDC 塞进 core

OIDC 需要：

- HTTPS/TLS；
- JWT/JWKS 验签；
- RSA/ECDSA 等密码学；
- 重定向/cookie/会话管理。

任何一项都要第三方 crate 或自己实现密码学，后者不可接受。  
所以：

- `ui-core` / `ui-cli` 继续 std-only；
- 企业 adapter 是独立进程/独立构建；
- core 只信任本机受信代理注入的**已验证**头。

## 2. 部署拓扑

```text
Browser
  │ HTTPS + OIDC cookie
  ▼
oauth2-proxy / 企业网关
  │ 仅 localhost / mTLS
  │ X-UI-User, X-UI-Roles, X-UI-Request-Id
  ▼
ui serve --trusted-proxy 127.0.0.1
```

强制条件：

- `ui serve` 只绑定 `127.0.0.1` 或受信网卡；
- 没有受信代理时，`X-UI-*` 头一律忽略；
- 代理必须剥离客户端自带的 `X-UI-*`；
- `/api/*` 写操作仍要求同源 + CSRF。

## 3. 身份契约

请求头：

```text
X-UI-User: alice@example.com
X-UI-UserId: 8f3a...
X-UI-Roles: operator,auditor
X-UI-Request-Id: ...
```

要求：

- `UserId` 是稳定主键，邮箱只用于显示；
- 角色只来自代理/组映射，不从请求体读取；
- 每次审批、review、revert、policy 变更都写 `actor`；
- 未知/空角色按 `viewer` 处理，不能默认 admin。

## 4. 角色矩阵

| 能力 | owner | admin | operator | auditor | viewer |
|---|---|---|---|---|---|
| 创建会话/任务 | ✓ | ✓ | ✓ | | |
| 审批工具 | ✓ | ✓ | ✓ | | |
| 写策略 | ✓ | ✓ | | | |
| 管理用户/角色 | ✓ | ✓ | | | |
| 审计导出 | ✓ | ✓ | | ✓ | |
| 查看脱敏会话 | ✓ | ✓ | ✓ | ✓ | ✓ |
| 查看正文 | ✓ | ✓ | 按策略 | 默认否 | 按策略 |
| review/revert | ✓ | ✓ | ✓ | | |

## 5. 代码接入点（待实现）

1. `serve` 增加 `--trusted-proxy <cidr>`；
2. 请求入口解析 `X-UI-*`，生成 `Actor {id, roles}`；
3. `/api/approve`、`/api/review`、`/api/revisit` 写 `actor`；
4. `/api/audit/export` 校验 `audit:read`；
5. journal 的 `review/apply`、`approval` 增加 `actorId`；
6. 所有 `/api/*` 默认拒绝无 actor 的写请求。

## 6. 验收测试

- 无代理头：写请求 401；
- viewer 调 `/api/review`：403；
- auditor 调 `/api/audit/export`：200，正文脱敏；
- operator 审批：journal actor=operator；
- 伪造 `X-UI-Roles` 直连：401/403；
- 代理重启：已有 cookie 仍可用（由代理负责）；
- 角色变更：下一请求生效，不需要重启 core。

## 7. 兼容零依赖

- core 不引入 OIDC/JWT/TLS crate；
- adapter 可用任意语言/依赖；
- 不装 adapter 时，本地单用户模式不变；
- `verify_independence.sh` 继续通过。

## 8. 明确不在 M1

- SCIM 自动同步；
- 多租户；
- 细粒度 ABAC；
- 审计 WORM/签名；
- 管理台。

这些属于 M2/M5。
