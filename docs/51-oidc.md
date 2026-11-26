# OIDC userinfo 适配层（可选 L2）

`adapters/oidc/oidc_proxy.py` 把「OIDC 已认证用户」翻译成 `ui serve` 认识的
`X-UI-*` 身份头。它是**适配层**，不是 OIDC 客户端。

## 1. 边界：做什么，不做什么

做：

- 接收 `Authorization: Bearer <access_token>`；
- 用 stdlib `urllib` 调 `OIDC_USERINFO_URL`（带超时，最多读 256 KiB）；
- 校验 userinfo 里有非空 `sub`；按 `OIDC_AUD_MODE` 校验 `aud` 包含
  `OIDC_CLIENT_ID`（默认 `strict`：配置了 client_id 时缺 `aud` 也拒绝）；
- `X-UI-User` 默认取 `sub`（`OIDC_IDENTITY=sub`）；`email` 只在
  `email_verified is True` 时作为可选的 `X-UI-Email` 展示头；
  `OIDC_IDENTITY=email` 只接受已验证邮箱，否则 401；
- 按 `OIDC_ROLES_CLAIM` 取角色（支持 `realm_access.roles` 点路径），归一成
  `X-UI-Roles` 的小写 CSV；
- 剥离客户端自带的全部 `X-UI-*`，注入自己的身份头与 `X-UI-Proxy-Secret`；
- 转发到 `UI_UPSTREAM_URL` 的 `ui serve`，状态码/响应头/正文（含 SSE）透传；
- `/api/events`（以及兼容端点 `/api/stream`）使用独立的 `OIDC_SSE_TIMEOUT`，
  普通请求使用 `OIDC_TIMEOUT`；
- 只监听回环地址；回环 upstream 允许 `http://`，非回环 upstream 只允许 `https://`。

不做：

- **不做 authorization code flow**：不生成/刷新/存储 token，不处理
  `code`/`state`/`nonce`/PKCE；
- 不签发 token，不代理登录页；
- 不在适配层验 JWT 签名：签名与过期由 IdP 的 userinfo 端点判定（适配层只信
  userinfo 的响应），因此 `OIDC_USERINFO_URL` 必须是 HTTPS（或本机测试用）；
- 不做角色映射表（角色来自 claim，矩阵在 `ui serve` 侧）；
- 不写 `users.json`，不改管理面配置。

一句话：**token 由现有 OIDC 客户端（oauth2-proxy、企业网关、前端 OIDC 库）
获取；本层只负责 userinfo → 身份头。**

## 2. 部署拓扑

```text
Browser
  │  HTTPS + OIDC cookie（由网关/客户端负责）
  ▼
oauth2-proxy / 企业网关 / SPA 的 OIDC 客户端
  │  Authorization: Bearer <access_token>
  ▼
oidc_proxy.py  （只监听 127.0.0.1:7789）
  │  X-UI-User / X-UI-Email / X-UI-Roles / X-UI-Proxy-Secret
  ▼
ui serve --trusted-proxy loopback --trusted-proxy-secret "$UI_PROXY_SECRET"
```

- `UI_PROXY_SECRET` 必须与 `ui serve --trusted-proxy-secret` 相同；缺密钥时两边
  都拒绝工作（`ui serve` 直接拒绝启动，适配层也拒绝启动）；
- 回环部署（默认）下适配层与 `ui serve` 之间是明文 HTTP，但只在回环地址上；
  upstream 换到非回环地址时必须改用 HTTPS（见第 5 节），否则启动失败；
- 浏览器不应该直接连 `ui serve`，也不应该知道 `X-UI-*` 的值。

## 3. 环境变量

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `OIDC_USERINFO_URL` | 是 | — | IdP userinfo 端点（生产必须 HTTPS） |
| `OIDC_CLIENT_ID` | 否 | 空 | 设置后按 `OIDC_AUD_MODE` 校验 userinfo 的 `aud`；为空时启动打印警告并跳过 |
| `OIDC_AUD_MODE` | 否 | `strict` | `strict`：缺 `aud` 或 `aud` 不含 client_id 都 401；`if-present`：有 `aud` 才校验，缺失放行；`off`：完全不校验（仅测试/迁就 IdP） |
| `OIDC_IDENTITY` | 否 | `sub` | `sub`：`X-UI-User=sub`；`email`：仅当 `email_verified is True` 时 `X-UI-User=email`，否则 401。生产建议 `sub` |
| `OIDC_ROLES_CLAIM` | 否 | `roles` | 角色声明名或点路径（`realm_access.roles`） |
| `UI_UPSTREAM_URL` | 否 | `http://127.0.0.1:7788` | `ui serve` 地址；回环可用 http，非回环必须 https |
| `UI_PROXY_SECRET` | 是 | — | 共享代理密钥，必须与 `ui serve` 一致 |
| `OIDC_LISTEN` | 否 | `127.0.0.1:7789` | 监听地址；非回环直接拒绝启动 |
| `OIDC_TIMEOUT` | 否 | `5` | 普通请求与 userinfo 的读超时秒数（0.1..60） |
| `OIDC_SSE_TIMEOUT` | 否 | `300` | `/api/events`、`/api/stream` 的读超时秒数（0.1..3600） |
| `OIDC_USERINFO_TIMEOUT` | 否 | — | 旧名，等价于 `OIDC_TIMEOUT`；两者都设置时 `OIDC_TIMEOUT` 优先 |

启动示例：

```bash
OIDC_USERINFO_URL=https://idp.example.com/oidc/userinfo \
OIDC_CLIENT_ID=ui-enterprise \
OIDC_AUD_MODE=strict \
OIDC_IDENTITY=sub \
OIDC_ROLES_CLAIM=realm_access.roles \
UI_UPSTREAM_URL=http://127.0.0.1:7788 \
UI_PROXY_SECRET="$PROXY_SECRET" \
OIDC_LISTEN=127.0.0.1:7789 \
OIDC_TIMEOUT=5 OIDC_SSE_TIMEOUT=300 \
python3 adapters/oidc/oidc_proxy.py
```

## 4. 请求处理

| 情形 | 响应 |
|---|---|
| 没有 `Authorization: Bearer` | 401 + `WWW-Authenticate: Bearer` |
| userinfo 返回 401/403 | 401（token 无效/过期） |
| userinfo 缺 `sub`、非 JSON | 401 |
| `aud` 不匹配；`strict` 下缺 `aud` | 401 |
| `if-present` 下缺 `aud` | 放行（有 `aud` 仍必须匹配） |
| `OIDC_AUD_MODE=off` | 跳过 `aud` 校验（仅兼容/测试） |
| `OIDC_IDENTITY=email` 但 email 缺失或 `email_verified is not True` | 401 |
| userinfo 网络错误 / 5xx / 重定向 | 502（fail closed，绝不猜测身份） |
| 请求体是 chunked 或超 1 MiB | 411 / 413 |
| userinfo 通过 | 转发；`ui serve` 的状态码与正文原样返回 |

身份头：

```text
X-UI-User: sub-1                  # 默认 OIDC_IDENTITY=sub；email 模式下是已验证邮箱
X-UI-Email: alice@example.com     # 可选展示头；仅 email_verified is True 时注入
X-UI-Roles: operator,auditor      # 小写、去重、只保留 [a-z0-9_.-]，最多 32 个
X-UI-Proxy-Secret: <UI_PROXY_SECRET>
```

- **默认 `OIDC_IDENTITY=sub`**：`sub` 是 IdP 内稳定且不可变的主键；`email` 可能被
  用户或管理员改写，未验证的邮箱更只是自述。`preferred_username` 不再作为身份主键；
- `OIDC_IDENTITY=email` 必须同时满足 `email_verified is True`（布尔 `true`，不是
  字符串 `"true"`），否则 401；这是为了兼容以 email 为键的 `.ui/users.json`，
  生产仍建议切到 `sub` 并迁移用户表；
- **不注入 `X-UI-UserId`**：`ui serve` 优先读它，注入 `sub` 会让用户 id 变成
  `sub`，与 `.ui/users.json` 里以 email 为键的记录对不上；映射键统一用 `X-UI-User`；
- 客户端自带的 `X-UI-*`（包括伪造的 `X-UI-Proxy-Secret` 与 `X-UI-Email`）先全部
  剥离再注入；
- `Authorization` 不向上游转发，避免 token 进入 `ui serve` 的日志/事件；
- 角色的判定仍由 `ui serve` 的矩阵 + 用户表决定：claim 里的 `owner` 只有在矩阵
  里 `owner` 有权限、且用户表没有覆盖时才生效。

转发头：

- 头字典统一按**小写键**去重、拼接；客户端自带的 `Host` 一律丢弃，`Host` 固定写成
  upstream 的 authority（`host:port`，默认端口不重复），不会出现两个 `Host`；
- 因为 `Host` 是 upstream 视角，`ui serve` 的 CSRF 检查（`Origin == http://<Host>`）
  只对**没有** `Sec-Fetch-Site` 的旧客户端会失配并 fail closed；现代浏览器的同源
  写请求带 `Sec-Fetch-Site: same-origin`，按既有逻辑放行。把代理暴露给旧客户端时
  需要另行确认写请求；
- 连接本身固定到 `UI_UPSTREAM_URL`，客户端无法用它改变转发目标。

SSE 与超时：

- `/api/events`（以及存在时的 `/api/stream`）改用 `OIDC_SSE_TIMEOUT`（默认 300s），
  路径按去 query 后的 path 精确匹配；
- 其它请求（含 userinfo）用 `OIDC_TIMEOUT`（默认 5s）；连接建立仍用普通超时，
  只有响应的读阶段切到 SSE 超时；
- 因此 SSE 不会被 5s 提前掐断，普通请求也不会借用 300s 的长超时。

## 5. 安全注意

- 适配层只监听 `127.0.0.1`/`localhost`/`::1`，非回环启动即失败；
- **upstream 传输**：回环 `http://127.0.0.1` / `http://localhost` 允许；只要 upstream
  不是回环地址，`http://` 启动即失败，必须用 `https://`。否则 Bearer token、
  身份头与 `X-UI-Proxy-Secret` 会明文穿过网络；
- **`aud` 与标准 IdP 的差距**：很多 IdP 的 `/userinfo` 只返回 `sub` 等声明，
  **不返回 `aud`**。默认 `strict` 因此会对这类 IdP 返回 401——这是有意的 fail
  closed。生产应优先在网关/资源服务器做 introspection 或 JWT 验签（本适配层
  不做签名校验），确实要依赖 userinfo 时再显式设 `OIDC_AUD_MODE=if-present`，
  并接受「IdP 不返回 aud 时无法校验 token 受众」的残余风险；不要用 `off` 上生产；
- **身份主键**：`sub` 在 IdP 内稳定；`email` 可变、可被管理员改写，未验证的邮箱
  只是自述。只有 `email_verified is True`（布尔）的邮箱才允许作为身份或展示头；
  生产建议 `OIDC_IDENTITY=sub`。切到 `sub` 会改变 `.ui/users.json` 的映射键，
  需要同步迁移用户表；`email` 模式仅为兼容现有 email 键而保留；
- userinfo 不跟随重定向，避免 `Authorization` 被带到别的地址；
- `/api/events` 用独立的长读超时（默认 300s）；普通请求保持 5s，避免把 SSE 的
  长超时误用到短请求上（也避免短超时提前断开 SSE）；
- 日志只写「方法 + 去 query 的路径 + 状态码」，不打印 token、Authorization 或
  身份字段；`/api/events?t=<token>` 这类 query 里的 token 也不会进日志；
- `X-UI-Proxy-Secret` 只存在适配层与 `ui serve` 之间；不写进响应；
- 生产环境必须用 HTTPS 连 IdP；适配层本身不终止 TLS，TLS 由前面的网关负责；
- 适配层不校验 JWT 签名，安全性依赖「userinfo 会拒绝无效 token」这一 OIDC
  事实；如果 IdP 的 userinfo 实现有缺陷，适配层无法补救。

## 6. 验证

### 6.1 适配层单测

```bash
python3 adapters/oidc/test_proxy.py
```

`test_proxy.py` 用本地 mock userinfo / mock `ui serve` 覆盖：

- 200：注入 `X-UI-User`/`X-UI-Email`/`X-UI-Roles`/`X-UI-Proxy-Secret`；
- 401：无 token、userinfo 401、缺 `sub`、`aud` 不匹配；
- `aud` 模式：`strict` 缺 `aud` → 401；`if-present` 缺 `aud` 放行、有 `aud` 不匹配
  仍 401；`off` 放行；
- 身份主键：默认 `sub`；未验证 email 不注入展示头；`OIDC_IDENTITY=email` 对
  `email_verified=false`、缺 email、字符串 `"true"` 都 401；布尔 `true` 可用；
- 502：userinfo 5xx（fail closed）；
- 上游 500 与 SSE 流式透传；
- SSE 超时：普通读超时 0.3s 时 `/api/events` 仍能收齐 3 段（间隔 0.6s）；
  同一配置下普通 `/slow` 仍按 0.3s 超时失败，证明两套超时没有混用；
- 客户端自带/重复的 `X-UI-*` 被剥离，`Authorization` 不转发；
- Host：客户端 `Host` 被丢弃，固定成 upstream authority，不产生重复 `Host`；
  转发头键统一小写；
- roles 声明是字符串/数组/嵌套点路径/`groups` 时的归一；
- `OIDC_LISTEN` 非回环、缺 `UI_PROXY_SECRET`、非回环 upstream 用 `http://` 时
  拒绝启动；回环 `http://localhost`/`http://[::1]` 允许；非回环 `https://` 允许；
- 真进程测试：日志里不出现 token 与代理密钥。

### 6.2 端到端（mock IdP → 代理 → `ui serve` → `/api/admin`）

```bash
python3 tools/oidc_e2e.py          # 直接跑；--ui 可指定二进制，--no-browser 跳过浏览器
bash tools/verify_oidc_e2e.sh      # 门禁包装器；没有 out/ui.exe 时先尝试构建
```

`tools/oidc_e2e.py` 自己启动三段进程，不联网、不调用真实模型：

1. 本进程内的 mock IdP（`127.0.0.1` 随机端口）：`/userinfo` 只认
   `owner-token`（owner@example.com / `email_verified: true` / `groups: ["owner"]`）和
   `viewer-token`（viewer@example.com / `email_verified: true` / `groups: ["viewer"]`），
   其它 token 一律 401；
2. `adapters/oidc/oidc_proxy.py`：`OIDC_ROLES_CLAIM=groups`、`OIDC_IDENTITY=email`
   （这条 e2e 要验证既有 `.ui/users.json` 的 email 映射，所以显式选用 email 身份；
   生产默认仍是 `sub`），上游指向下面这个 `ui serve`，共享密钥随机生成；
3. `out/ui.exe serve --trusted-proxy loopback --trusted-proxy-secret <随机> --token off`，
   在 `out/` 下的临时工作区跑 `--mock` 剧本（gitignored，结束删除）；结束后（含失败
   路径）Windows 用 `taskkill /T`、POSIX 用 `terminate` 收掉所有进程。

HTTP 断言：

| 场景 | 期望 |
|---|---|
| 无 `Authorization` | 401 |
| 无效 token | 401 |
| viewer 读 `/api/admin/rbac` | 403 |
| owner 读 `/api/admin/rbac` | 200，矩阵含 `owner` |
| viewer token + 伪造 `X-UI-User`/`X-UI-Roles: owner`/`X-UI-Proxy-Secret` | 仍按 viewer → 403（证明剥离） |
| 只有伪造 `X-UI-*`、无 Bearer | 401 |
| owner `PUT /api/admin/roles` | 200 |
| owner `PUT /api/admin/users`（新增 operator） | 200，随后 `GET` 能看到 |
| viewer `PUT /api/admin/users` | 403 |
| 工作区 `.ui/journal.jsonl` | 出现 `rbac/role-update`、`rbac/user-update`，actor 是 owner@example.com |
| mock IdP / 代理 / `ui serve` 日志 | 不含 owner/viewer/无效 token，也不含代理密钥 |

浏览器部分是**可选**断言：找到 Chrome/Edge 时，用 CDP
`Network.setExtraHTTPHeaders` 注入 `Authorization: Bearer owner-token`，打开
`/admin`，断言 `userForm`/`matrixBody` 存在；若注入生效，再确认矩阵/身份区出现
`owner`。没有 Chrome/Edge、或 CDP 注入不稳定时，浏览器部分明确 SKIP/WARN，
HTTP 断言仍是硬门禁。

SKIP 规则：

- 没有 `out/ui.exe`（也没有 `--ui` / `UI_EXE`）→ 明确 SKIP，并提示先
  `bash tools/build_windows.sh`；不再假装链路验证过；
- 没有 Chrome/Edge → 只 SKIP 浏览器，HTTP e2e 必须全绿；
- `tools/verify_oidc_e2e.sh` 在缺二进制时会先尝试本机或 WSL 构建，构建环境不可用
  才 SKIP（退出码 0）。

### 6.3 mock IdP 与真实 IdP 的差距

这条 e2e 证明的是「适配层 → 受信代理 → 管理 API」的接线和权限边界，**不是**
OIDC 协议一致性。差距清单：

| 维度 | mock IdP | 真实 IdP |
|---|---|---|
| token 真伪 | 两张硬编码字符串，非 JWT | 签名、`exp`/`nbf`/`iss` 由 userinfo 端点判定 |
| 发现/元数据 | 没有 `.well-known`、JWKS、issuer | 可能要求 discovery、JWKS、issuer 校验 |
| 传输 | 明文 HTTP，仅回环 | 生产必须 HTTPS；适配层不终止 TLS |
| `aud` | 固定 `ui-oidc-e2e`（字符串） | 常不返回；也可能是数组、多 audience，或只在 introspection/JWT 里 |
| claims | 只有 `sub`/`email`/`email_verified`/`aud`/`groups` | 常见 `preferred_username`、嵌套 `realm_access.roles`、空/缺失 `sub`、`email_verified` 缺失或为字符串（单测覆盖归一，e2e 只跑 `groups`） |
| 故障注入 | 正常响应 + 401 | 网络错误、5xx、超时、重定向、坏 JSON（单测覆盖，e2e 未注入） |
| 登录流程 | 无 | authorization code + PKCE、refresh、logout、cookie 由上游客户端负责，本适配层不做 |
| 多用户/并发 | 两个固定用户，串行请求 | 真实用户规模、并发、限流、token 撤销时延 |
| 角色来源 | `groups` 声明 | 可能是 `roles`、`realm_access.roles` 或自定义 claim；矩阵仍由 `ui serve` 决定 |

因此上线前还应在预发环境用真实 IdP 复跑这张表，尤其是 HTTPS、真实 token 过期、
`aud` 形状与角色 claim 的实际字段名。

## 7. 与文档 41/47 的关系

- [docs/41](41-enterprise-m1.md)：为什么 OIDC/TLS/JWT 不进入 `ui-core`，以及
  `ui serve` 只信任受信代理注入的已认证身份；
- [docs/47](47-admin.md)：用户表 `.ui/users.json`、角色矩阵、owner 约束与管理 API；
- 本文件：适配层如何把 OIDC token 变成受信代理身份头，以及明确不做的部分
  （code flow / token 签发 / JWT 验签）。
