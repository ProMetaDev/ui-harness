# OIDC userinfo 适配层（可选 L2）

`adapters/oidc/oidc_proxy.py` 把「OIDC 已认证用户」翻译成 `ui serve` 认识的
`X-UI-*` 身份头。它是**适配层**，不是 OIDC 客户端。

## 1. 边界：做什么，不做什么

做：

- 接收 `Authorization: Bearer <access_token>`；
- 用 stdlib `urllib` 调 `OIDC_USERINFO_URL`（带超时，最多读 256 KiB）；
- 校验 userinfo 里有非空 `sub`；可选校验 `aud` 包含 `OIDC_CLIENT_ID`；
- 取 `email` → `preferred_username` → `sub` 作为 `X-UI-User`；
- 按 `OIDC_ROLES_CLAIM` 取角色（支持 `realm_access.roles` 点路径），归一成
  `X-UI-Roles` 的小写 CSV；
- 剥离客户端自带的全部 `X-UI-*`，注入自己的身份头与 `X-UI-Proxy-Secret`；
- 转发到 `UI_UPSTREAM_URL` 的 `ui serve`，状态码/响应头/正文（含 SSE）透传；
- 只监听回环地址。

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
  │  X-UI-User / X-UI-Roles / X-UI-Proxy-Secret
  ▼
ui serve --trusted-proxy loopback --trusted-proxy-secret "$UI_PROXY_SECRET"
```

- `UI_PROXY_SECRET` 必须与 `ui serve --trusted-proxy-secret` 相同；缺密钥时两边
  都拒绝工作（`ui serve` 直接拒绝启动，适配层也拒绝启动）；
- 适配层与 `ui serve` 之间是明文 HTTP，但只在回环地址上；
- 浏览器不应该直接连 `ui serve`，也不应该知道 `X-UI-*` 的值。

## 3. 环境变量

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `OIDC_USERINFO_URL` | 是 | — | IdP userinfo 端点（生产必须 HTTPS） |
| `OIDC_CLIENT_ID` | 否 | 空 | 设置后校验 userinfo 的 `aud` 包含它；为空时启动打印警告并跳过 |
| `OIDC_ROLES_CLAIM` | 否 | `roles` | 角色声明名或点路径（`realm_access.roles`） |
| `UI_UPSTREAM_URL` | 否 | `http://127.0.0.1:7788` | `ui serve` 地址；只允许回环 |
| `UI_PROXY_SECRET` | 是 | — | 共享代理密钥，必须与 `ui serve` 一致 |
| `OIDC_LISTEN` | 否 | `127.0.0.1:7789` | 监听地址；非回环直接拒绝启动 |
| `OIDC_USERINFO_TIMEOUT` | 否 | `5` | userinfo 超时秒数（0.1..60） |

启动示例：

```bash
OIDC_USERINFO_URL=https://idp.example.com/oidc/userinfo \
OIDC_CLIENT_ID=ui-enterprise \
OIDC_ROLES_CLAIM=realm_access.roles \
UI_UPSTREAM_URL=http://127.0.0.1:7788 \
UI_PROXY_SECRET="$PROXY_SECRET" \
OIDC_LISTEN=127.0.0.1:7789 \
python3 adapters/oidc/oidc_proxy.py
```

## 4. 请求处理

| 情形 | 响应 |
|---|---|
| 没有 `Authorization: Bearer` | 401 + `WWW-Authenticate: Bearer` |
| userinfo 返回 401/403 | 401（token 无效/过期） |
| userinfo 缺 `sub`、非 JSON、`aud` 不匹配 | 401 |
| userinfo 网络错误 / 5xx / 重定向 | 502（fail closed，绝不猜测身份） |
| 请求体是 chunked 或超 1 MiB | 411 / 413 |
| userinfo 通过 | 转发；`ui serve` 的状态码与正文原样返回 |

身份头：

```text
X-UI-User: alice@example.com      # email 优先，其次 preferred_username，最后 sub
X-UI-Roles: operator,auditor      # 小写、去重、只保留 [a-z0-9_.-]，最多 32 个
X-UI-Proxy-Secret: <UI_PROXY_SECRET>
```

- **不注入 `X-UI-UserId`**：`ui serve` 优先读它，注入 `sub` 会让用户 id 变成
  `sub`，与 `.ui/users.json` 里以 email 为键的记录对不上；
- 客户端自带的 `X-UI-*`（包括伪造的 `X-UI-Proxy-Secret`）先全部剥离再注入；
- `Authorization` 不向上游转发，避免 token 进入 `ui serve` 的日志/事件；
- 角色的判定仍由 `ui serve` 的矩阵 + 用户表决定：claim 里的 `owner` 只有在矩阵
  里 `owner` 有权限、且用户表没有覆盖时才生效。

`Host` 原样转发（客户端视角的 Host），因为 `ui serve` 的 CSRF 检查按
`Origin == http://<Host>` 比对；连接本身仍固定到 `UI_UPSTREAM_URL`。

## 5. 安全注意

- 适配层只监听 `127.0.0.1`/`localhost`/`::1`，非回环启动即失败；
- userinfo 不跟随重定向，避免 `Authorization` 被带到别的地址；
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

- 200：注入 `X-UI-User`/`X-UI-Roles`/`X-UI-Proxy-Secret`；
- 401：无 token、userinfo 401、缺 `sub`、`aud` 不匹配；
- 502：userinfo 5xx（fail closed）；
- 上游 500 与 SSE 流式透传；
- 客户端自带/重复的 `X-UI-*` 被剥离，`Authorization` 不转发；
- roles 声明是字符串/数组/嵌套点路径/`groups` 时的归一；
- `OIDC_LISTEN` 非回环、缺 `UI_PROXY_SECRET`、上游非回环时拒绝启动；
- 真进程测试：日志里不出现 token 与代理密钥。

### 6.2 端到端（mock IdP → 代理 → `ui serve` → `/api/admin`）

```bash
python3 tools/oidc_e2e.py          # 直接跑；--ui 可指定二进制，--no-browser 跳过浏览器
bash tools/verify_oidc_e2e.sh      # 门禁包装器；没有 out/ui.exe 时先尝试构建
```

`tools/oidc_e2e.py` 自己启动三段进程，不联网、不调用真实模型：

1. 本进程内的 mock IdP（`127.0.0.1` 随机端口）：`/userinfo` 只认
   `owner-token`（owner@example.com / `groups: ["owner"]`）和
   `viewer-token`（viewer@example.com / `groups: ["viewer"]`），其它 token 一律 401；
2. `adapters/oidc/oidc_proxy.py`：`OIDC_ROLES_CLAIM=groups`，上游指向下面这个
   `ui serve`，共享密钥随机生成；
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
| `aud` | 固定 `ui-oidc-e2e` | 可能是字符串/数组、多 audience，未覆盖全部形状 |
| claims | 只有 `sub`/`email`/`aud`/`groups` | 常见 `preferred_username`、嵌套 `realm_access.roles`、空/缺失 `sub`（单测覆盖归一，e2e 只跑 `groups`） |
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
