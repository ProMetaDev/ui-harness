# 企业 M1：身份与 RBAC 的零依赖边界

> 状态：边界设计 + 最小写权限 + M6 RBAC 管理闭环（配置/API/审计）。
> OIDC adapter / 可写管理台表单尚未实现。
>
> 已实现：`ui serve --trusted-proxy loopback --trusted-proxy-secret <secret>`、
> `X-UI-Proxy-Secret` 共享密钥校验、`X-UI-User/UserId/Roles` 解析、
> 无 actor 401、viewer/auditor 写操作 403、auditor 可读 `/api/reviews`；
> 受信代理模式缺密钥时**拒绝启动**（fail closed）。
>
> M6 新增：`.ui/rbac.json` 角色矩阵/用户映射（缺失用内置矩阵，损坏拒绝启动）、
> `GET /api/admin/rbac`、`PUT /api/admin/roles`（owner）、
> `PUT/DELETE /api/admin/users`（manage），每次变更写
> `rbac/role-update` / `rbac/user-update` 审计。
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

矩阵的唯一来源是 `.ui/rbac.json`（不存在时用下表的内置默认值）：

```json
{
  "roles": {
    "viewer":   {"write": false, "audit": false, "manage": false},
    "operator": {"write": true,  "audit": false, "manage": false},
    "auditor":  {"write": false, "audit": true,  "manage": false},
    "admin":    {"write": true,  "audit": true,  "manage": true},
    "owner":    {"write": true,  "audit": true,  "manage": true}
  },
  "users": {"alice@example.com": "operator"}
}
```

| 能力 | 判定字段 | owner | admin | operator | auditor | viewer |
|---|---|---|---|---|---|---|
| 写请求（`POST`/`PUT`/`DELETE`，管理面除外） | `write` | ✓ | ✓ | ✓ | | |
| 读 `/api/reviews`、`/api/audit/export` | `audit` | ✓ | ✓ | | ✓ | |
| 读 `GET /api/admin/rbac` | `audit` 或 `manage` | ✓ | ✓ | | ✓ | |
| 改角色矩阵 `PUT /api/admin/roles` | 角色名 `owner` | ✓ | | | | |
| 改用户映射 `PUT/DELETE /api/admin/users` | `manage` | ✓ | ✓ | | | |

约定：

- 角色名大小写不敏感，入库统一小写；`write/audit/manage` 必须是 JSON 布尔值，
  缺字段、字符串 `"true"`、未知字段都按配置错误拒绝；
- `users` 映射**只覆盖角色**：用户仍必须先过受信代理密钥（或本地 token），
  映射不能绕过任何一道鉴权；未映射用户按 `X-UI-Roles` 判定；
- 用户映射到的角色必须存在于同一份 `roles`；删除角色时若还有用户引用，写请求被
  400 拒绝，避免留下指向未知角色的映射；
- 文件缺失 = 内置矩阵；文件不可读/JSON 非法/字段非法 = `serve` **拒绝启动**，
  运行期内存态若为 `Invalid` 则所有 `/api/admin/*` 返回 503（fail closed）；
- 写盘顺序：临时文件 → `rename` 原子替换 → 更新内存态 → 写审计；
  任何一步失败都不会让响应声称成功。

## 6. RBAC 管理 API

| 方法与路径 | 权限 | 说明 |
|---|---|---|
| `GET /api/admin/rbac` | `audit` 或 `manage` | 返回 `roles`、`users`、`source`（固定 `.ui/rbac.json`） |
| `PUT /api/admin/roles` | owner | `{"roles": {...}}` 整份替换，或 `{"name":"operator","write":true,"audit":false,"manage":false}` 改一个 |
| `PUT /api/admin/users` | `manage` | `{"id":"alice@example.com","role":"operator"}`；`role:""` 等于删除映射 |
| `DELETE /api/admin/users?id=...` | `manage` | 删除映射；不存在返回 404 |

- 所有 `/api/admin/*` 都要求受信代理 actor：无 actor 一律 401，即使本地模式；
- 变更写当前 session 的 journal（不是固定的 default）：
  `rbac/role-update` 带完整角色矩阵，`rbac/user-update` 带 `id/role/action/actor`；
- 响应里的 `journalSeq` 是审计事件序号，便于外部核对；
- 页面 `admin.html` 只读展示矩阵与用户映射；写操作只需 curl/API，不要求表单。

## 7. 代码接入点（部分已实现）

1. `serve` 增加 `--trusted-proxy loopback|<ip>` 与 `--trusted-proxy-secret <secret>`
   （已实现 loopback/精确 IP；缺密钥拒绝启动）；
2. 请求入口先校验 `X-UI-Proxy-Secret`（定长比较，不匹配 401），再解析 `X-UI-*`
   生成 `Actor {id, roles}`，最后用 `.ui/rbac.json` 的 `users` 覆盖角色（已实现）；
3. `/api/approve`、`/api/review` 写 `actor`（已实现；approval/answered 记录受信 actor）；
4. `/api/audit/export` 按矩阵 `audit` 校验（已实现）；
5. journal 的 `review/apply`、`approval` 增加 `actorId`（已实现）；
6. `/api/admin/*` 按矩阵判定，角色变更即时生效，不需要重启 core（已实现）。

## 8. 验收测试

- 无代理头：写请求 401；
- `--trusted-proxy` 缺 `--trusted-proxy-secret`：进程启动即报错退出（fail closed）；
- 缺 `X-UI-Proxy-Secret` 或值不对：任意路径 401（含 `/`、`/snapshot`、`/api/*`）；
- 带对代理密钥但无身份头：`/api/*` 401；
- viewer 调 `/api/review`：403；viewer 读 `/api/admin/rbac`：403；
- auditor 读 `/api/admin/rbac`：200；operator 改角色：403；
- owner 改角色：200，文件已原子落盘，journal 出现 `rbac/role-update`；
- `alice` 在 `users` 里映射为 operator：同一 `X-UI-User` 下即使头里写 viewer 也可写；
  反向映射（头里写 owner、映射为 viewer）必须降权；
- 损坏的 `.ui/rbac.json`：启动报错；已加载为 `Invalid` 的 hub 对管理面返回 503；
- 事件流、journal、`rbac.json` 与响应正文中不出现代理密钥原文；
- 角色变更：下一请求生效，不需要重启 core。

## 9. 兼容零依赖

- core 不引入 OIDC/JWT/TLS crate；
- 共享密钥比较是手写的定长比较（std），没有引 crypto crate；
- RBAC 解析复用 `ui_core::json`，落盘复用现有 `Journal`，新增代码仍是 std-only；
- adapter 可用任意语言/依赖；
- 不装 adapter 时，本地单用户模式不变；
- `verify_independence.sh` 继续通过。

## 10. 明确不在 M1/M6

- SCIM 自动同步；
- 多租户；
- 细粒度 ABAC；
- 审计 WORM/签名；
- 管理台的可视化角色编辑表单（当前只有只读表 + API）。

这些属于后续里程碑。
