# 企业 M1：身份与 RBAC 的零依赖边界

> 状态：边界设计 + 最小写权限 + M6 RBAC 管理闭环（配置/API/可写管理台/用户存储/审计）。
> OIDC userinfo 适配层已提供（`adapters/oidc/`）；authorization code flow 不在本适配层内。
>
> 已实现：`ui serve --trusted-proxy loopback --trusted-proxy-secret <secret>`、
> `X-UI-Proxy-Secret` 共享密钥校验、`X-UI-User/UserId/Roles` 解析、
> 无 actor 401、viewer/auditor 写操作 403、auditor 可读 `/api/reviews`；
> 受信代理模式缺密钥时**拒绝启动**（fail closed）。
>
> M6 新增：`.ui/rbac.json` 角色矩阵（缺失用内置矩阵，损坏拒绝启动）、
> `.ui/users.json` 用户表（缺失回退 `rbac.json` 的 users；损坏 fail closed；
> `disabled` 一律 403）、`GET /api/admin/rbac`、`GET /api/admin/users`、
> `PUT /api/admin/roles`（owner）、`PUT/DELETE /api/admin/users`（manage，
> owner 记录仍归 owner），写操作由一把管理写锁串行，审计按
> `*-intent` → 落盘/换内存 → `rbac/role-update` / `rbac/user-update`（含
> `intentSeq`）记账，落盘失败补 `rbac/update-error`；`/admin` 提供可写的
> 矩阵 checkbox 表与用户表单。
>
> 结论：**OIDC/TLS/JWT 不进入 `ui-core`。** 核心只接受来自受信代理的已认证身份；
> 企业 adapter 负责 OIDC、cookie、组映射。这样不破坏 docs/38 的 L0 零依赖。
> OIDC 边界与部署见 [docs/51](51-oidc.md)。

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

矩阵的唯一来源是 `.ui/rbac.json`（不存在时用下表的内置默认值）。用户记录自 M6
起在 `.ui/users.json`；`rbac.json` 的 `users` 只在 `users.json` 缺失时作为旧格式
回退：

```json
{
  "roles": {
    "viewer":   {"write": false, "audit": false, "manage": false},
    "operator": {"write": true,  "audit": false, "manage": false},
    "auditor":  {"write": false, "audit": true,  "manage": false},
    "admin":    {"write": true,  "audit": true,  "manage": true},
    "owner":    {"write": true,  "audit": true,  "manage": true}
  }
}
```

```json
{
  "users": [
    {"id": "alice@example.com", "role": "operator", "disabled": false, "display": "Alice"}
  ]
}
```

| 能力 | 判定字段 | owner | admin | operator | auditor | viewer |
|---|---|---|---|---|---|---|
| 写请求（`POST`/`PUT`/`DELETE`，管理面除外） | `write` | ✓ | ✓ | ✓ | | |
| 读 `/api/reviews`、`/api/audit/export` | `audit` | ✓ | ✓ | | ✓ | |
| 读 `GET /api/admin/rbac` / `GET /api/admin/users` | `audit` 或 `manage` | ✓ | ✓ | | ✓ | |
| 改角色矩阵 `PUT /api/admin/roles` | 角色名 `owner` | ✓ | | | | |
| 改用户 `PUT/DELETE /api/admin/users` | `manage`（owner 记录需 `owner`） | ✓ | ✓ | | | |
| 读 `/api/metrics`（只读运维指标） | 无额外能力（任何通过身份校验的角色） | ✓ | ✓ | ✓ | ✓ | ✓ |
| 读 `/api/file`（只读工作区预览） | 无额外能力；路径守卫与敏感文件策略仍生效 | ✓ | ✓ | ✓ | ✓ | ✓ |

约定：

- 角色名大小写不敏感，入库统一小写；`write/audit/manage` 必须是 JSON 布尔值，
  缺字段、字符串 `"true"`、未知字段都按配置错误拒绝；
- 用户表覆盖角色，但**只覆盖角色**：用户仍必须先过受信代理密钥（或本地 token），
  映射不能绕过任何一道鉴权；未映射用户按 `X-UI-Roles` 判定；
- `.ui/users.json` 存在时是用户表的唯一事实（见 docs/47）；不存在才回退
  `rbac.json` 的 `users` 映射；
- 用户映射到的角色必须存在于同一份 `roles`；删除角色时若还有用户引用，写请求被
  400 拒绝，避免留下指向未知角色的映射；
- 文件缺失 = 内置矩阵；文件不可读/JSON 非法/字段非法 = `serve` **拒绝启动**，
  运行期内存态若为 `Invalid` 则所有 `/api/*` 返回 503（fail closed）；
- owner 不变量：矩阵必须始终存在 `owner` 角色，且 `owner.manage=true`；整份替换
  删掉 owner、或把 owner 的 `manage` 清成 false 都会被 400 拒绝。授予/修改/撤销
  映射到 owner 的用户仍只有 owner 本人能做；
- 管理写操作由 `Hub` 内独立的 `admin_write` 锁串行（不复用 `rbac` / `users`
  读写锁，避免死锁）：读当前配置 → 校验 → 写 intent 审计 → 原子落盘 + 换内存
  → 写最终审计。读请求只在换内存的短临界区碰 `rbac/users`，不会被落盘或审计 IO
  挡住；两个并发更新不会各自基于旧快照把对方覆盖掉；
- 审计顺序：先 `rbac/role-update-intent` / `rbac/user-update-intent`（actor +
  变更摘要），成功落盘后写 `rbac/role-update` / `rbac/user-update` 并带
  `intentSeq`；落盘失败写 `rbac/update-error`（带 `intentSeq` 和错误原文）。
  intent 写不进去就不执行变更，不会出现"已落盘但无审计"的静默成功。

## 6. 管理 API

| 方法与路径 | 权限 | 说明 |
|---|---|---|
| `GET /api/admin/rbac` | `audit` 或 `manage` | 返回 `roles`、生效的 `users`、`source`、`usersSource` |
| `GET /api/admin/users` | `audit` 或 `manage` | 返回 `users`（含 `disabled/display`）、`roles`、`source` |
| `PUT /api/admin/roles` | owner | `{"roles": {...}}` 整份替换，或 `{"name":"operator","write":true,"audit":false,"manage":false}` 改一个 |
| `PUT /api/admin/users` | `manage` | `{"id":"alice@example.com","role":"operator","disabled":false,"display":"Alice"}`；`role:""` 等于删除（等价 `DELETE`） |
| `DELETE /api/admin/users?id=...` | `manage` | 删除用户；不存在返回 404；删除 owner 需 owner |

- 所有 `/api/admin/*` 都要求受信代理 actor：无 actor 一律 401，即使本地模式；
- 变更写当前 session 的 journal（不是固定的 default）：先写 `-intent`，成功落盘
  后再写 `rbac/role-update` / `rbac/user-update`（带 `intentSeq`）；
  `rbac/role-update` 带完整角色矩阵，`rbac/user-update` 带 `id/role/action/actor`；
- 落盘失败写 `rbac/update-error`（`target` = `roles`/`users`、`intentSeq`、`error`），
  响应 `applied:false`，内存保持旧状态；
- 响应里的 `journalSeq` 是最终审计事件序号，`intentSeq` 是对应的 intent 序号，
  便于外部核对；
- 页面 `admin.html` 可直接保存角色矩阵、新增/编辑/删除用户；未授权时按钮禁用并
  显示 403 原因。

## 7. 代码接入点

1. `serve` 增加 `--trusted-proxy loopback|<ip>` 与 `--trusted-proxy-secret <secret>`
   （已实现 loopback/精确 IP；缺密钥拒绝启动）；
2. 请求入口先校验 `X-UI-Proxy-Secret`（定长比较，不匹配 401），再解析 `X-UI-*`
   生成 `Actor {id, roles, disabled}`，最后按用户表覆盖角色（`users.json` 优先，
   缺失回退 `rbac.json#users`；已实现）；停用用户在所有路径 403；
3. `/api/approve`、`/api/review` 写 `actor`（已实现；approval/answered 记录受信 actor）；
4. `/api/audit/export` 按矩阵 `audit` 校验（已实现）；
5. journal 的 `review/apply`、`approval` 增加 `actorId`（已实现）；
6. `/api/admin/*` 按矩阵判定，角色变更即时生效，不需要重启 core（已实现）；
7. `adapters/oidc/oidc_proxy.py` 做 Bearer → userinfo → `X-UI-*` 注入
   （已实现；不含 code flow，见 docs/51）。

## 8. 验收测试

- 无代理头：写请求 401；
- `--trusted-proxy` 缺 `--trusted-proxy-secret`：进程启动即报错退出（fail closed）；
- 缺 `X-UI-Proxy-Secret` 或值不对：任意路径 401（含 `/`、`/snapshot`、`/api/*`）；
- 带对代理密钥但无身份头：`/api/*` 401；
- viewer 调 `/api/review`：403；viewer 读 `/api/admin/rbac`：403；
- viewer 读 `/api/admin/users`：403；auditor 读：200；auditor 写用户：403；
- auditor 读 `/api/admin/rbac`：200；operator 改角色：403；
- owner 改角色：200，文件已原子落盘，journal 出现 `rbac/role-update`；
- `alice` 在 `users` 里映射为 operator：同一 `X-UI-User` 下即使头里写 viewer 也可写；
  反向映射（头里写 owner、映射为 viewer）必须降权；
- 用户表：新增写 `users.json`、删除写回空数组、`disabled` 用户任意路径 403、
  第一次写时旧 `rbac.json#users` 映射被迁移；
- 损坏的 `.ui/rbac.json` / `.ui/users.json`：启动报错；已加载为 `Invalid` 的 hub
  对 `/api/*` 返回 503；
- 事件流、journal、`rbac.json`、`users.json` 与响应正文中不出现代理密钥原文；
- 角色变更：下一请求生效，不需要重启 core；
- 并发两个角色/用户更新：最终文件同时包含两笔（管理写锁串行，不丢更新）；
- owner 不变量：整表删掉 owner / 把 owner `manage` 改成 false 一律 400，内存与
  磁盘保持旧矩阵；
- 落盘失败（目标路径被目录占住）：响应 `applied:false`，journal 依次出现
  `<kind>-intent` 与 `rbac/update-error`，不会静默成功；
- viewer 读 `/api/metrics` 与普通 `/api/file` 为 200；敏感文件仍 403；
- `/api/lab/report` 对 URL 编码的 `..`、反斜杠、绝对路径、包内路径返回错误，
  只接受单段包名。

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
- OIDC authorization code flow / token 签发（token 由现有 OIDC 客户端获取，
  `adapters/oidc/` 只做 userinfo 适配）；
- 管理台的分页与批量导入（表单只做单条增删改）。

这些属于后续里程碑。
