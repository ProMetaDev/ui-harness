# 企业 M1：身份与 RBAC 的零依赖边界

> 状态：边界设计 + 最小写权限实现。OIDC adapter / 管理台尚未实现。
>
> 已实现：`ui serve --trusted-proxy loopback --trusted-proxy-secret <secret>`、
> `X-UI-Proxy-Secret` 共享密钥校验、`X-UI-User/UserId/Roles` 解析、
> 无 actor 401、viewer/auditor 写操作 403、auditor 可读 `/api/reviews`；
> 受信代理模式缺密钥时**拒绝启动**（fail closed）。
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
  │ X-UI-Proxy-Secret, X-UI-User, X-UI-Roles, X-UI-Request-Id
  ▼
ui serve --trusted-proxy 127.0.0.1 --trusted-proxy-secret "$PROXY_SECRET"
```

强制条件：

- `ui serve` 只绑定 `127.0.0.1` 或受信网卡；
- **打开 `--trusted-proxy` 就必须同时给非空 `--trusted-proxy-secret`**，否则直接
  报错退出（fail closed），不会退回"只看源地址"；
- 每个受信代理请求必须带 `X-UI-Proxy-Secret`，与启动配置相等，否则一律 401
  （页面、`/snapshot`、`/api/*` 都算；`/api/ready` 也不例外）；
- 没有受信代理时，`X-UI-*` 头一律忽略；
- 代理必须剥离客户端自带的 `X-UI-*`（包括 `X-UI-Proxy-Secret`），再注入自己的；
- `/api/*` 写操作仍要求同源 + CSRF；
- 密钥只存在代理与 core 之间：不打印、不进事件、不进 journal（启动横幅只说
  "要求 `X-UI-Proxy-Secret`"，不显示值）。

## 3. 身份契约

请求头：

```text
X-UI-Proxy-Secret: <与 --trusted-proxy-secret 相同的共享密钥>
X-UI-User: alice@example.com
X-UI-UserId: 8f3a...
X-UI-Roles: operator,auditor
X-UI-Request-Id: ...
```

要求：

- `X-UI-Proxy-Secret` 先于身份解析校验；缺失/不匹配直接 401，不再看 roles；
- 密钥比较是定长的（不按字节提前返回），错误响应不区分"缺"与"错"；
- `UserId` 是稳定主键，邮箱只用于显示；
- 角色只来自代理/组映射，不从请求体读取；
- 每次审批、review、revert、policy 变更都写 `actor`；
- 未知/空角色按 `viewer` 处理，不能默认 admin。

## 4. 为什么 loopback 本身不是身份

`--trusted-proxy loopback` 的原语义是"来自 127.0.0.1 的请求是代理发的"。
这句话在单机开发环境成立，在真正的企业部署里不成立：同一台机器上的任何进程
（另一个服务、被攻陷的构建脚本、用户自己起的程序）都能连上 127.0.0.1 并伪造
`X-UI-User/X-UI-Roles`，从而拿到 operator 甚至 owner。

所以 M1 的信任边界改成"源地址 + 共享密钥"，两层都过才解析身份：

1. 启动时 `--trusted-proxy` 与 `--trusted-proxy-secret` 必须成对出现，密钥为空
   拒绝启动；
2. 每个请求先比对 `X-UI-Proxy-Secret`，再读取 `X-UI-*` 身份头。

密钥轮换：改两边的配置并重启 core；没有密钥版本协商，也没有"旧密钥宽限期"——
宁可短暂 401，也不接受一个不再受控的密钥。密钥建议由 secret manager 注入
环境变量或权限 0600 的启动脚本，不要写进仓库、进程命令行历史或 issue。

## 5. 角色矩阵

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

## 6. 代码接入点（部分已实现）

1. `serve` 增加 `--trusted-proxy loopback|<ip>` 与 `--trusted-proxy-secret <secret>`
   （已实现 loopback/精确 IP；缺密钥拒绝启动）；
2. 请求入口先校验 `X-UI-Proxy-Secret`（定长比较，不匹配 401），再解析 `X-UI-*`
   生成 `Actor {id, roles}`（已实现）；
3. `/api/approve`、`/api/review` 写 `actor`（已实现；approval/answered 记录受信 actor）；
4. `/api/audit/export` 校验 auditor/admin（已实现）；
5. journal 的 `review/apply`、`approval` 增加 `actorId`（已实现）；
6. 所有 `/api/*` 默认拒绝无 actor 的写请求；受信代理模式连页面请求都要求密钥。

## 7. 验收测试

- 无代理头：写请求 401；
- `--trusted-proxy` 缺 `--trusted-proxy-secret`：进程启动即报错退出（fail closed）；
- 缺 `X-UI-Proxy-Secret` 或值不对：任意路径 401（含 `/`、`/snapshot`、`/api/*`）；
- 带对代理密钥但无身份头：`/api/*` 401；
- viewer 调 `/api/review`：403；
- auditor 调 `/api/audit/export`：200，正文脱敏；
- operator 审批：journal actor=operator；
- 本机进程伪造 `X-UI-Roles` 直连（无密钥/错密钥）：401，而不是 403；
- 事件流与 journal 中不出现代理密钥原文；
- 代理重启：已有 cookie 仍可用（由代理负责）；
- 角色变更：下一请求生效，不需要重启 core。

## 8. 兼容零依赖

- core 不引入 OIDC/JWT/TLS crate；
- 共享密钥比较是手写的定长比较（std），没有引 crypto crate；
- adapter 可用任意语言/依赖；
- 不装 adapter 时，本地单用户模式不变；
- `verify_independence.sh` 继续通过。

## 9. 明确不在 M1

- SCIM 自动同步；
- 多租户；
- 细粒度 ABAC；
- 审计 WORM/签名；
- 管理台。

这些属于 M2/M5。
