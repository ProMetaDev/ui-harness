# `/admin` 管理面

> M6 的管理面：零依赖原生 HTML/JS，不引入前端框架或 CDN。
> 页面**可写**：角色矩阵用 checkbox 表格保存，用户用新增/编辑/删除表单；
> 每次变更落 `.ui/users.json` 或 `.ui/rbac.json`，并写当前 session journal。
>
> 不能做的操作在页面上直接禁用并写明原因（缺 `manage` / 缺 `owner` / 没有
> 受信代理身份），而不是让人点下去才看到 403。

## 访问

```text
http://127.0.0.1:7788/admin
```

- 与其他页面一样受 token / 受信代理约束；
- 本地单用户模式显示「本地单用户模式」，管理 API 一律 401；
- `--trusted-proxy` 模式显示 `X-UI-User` 与 roles（该模式要求每个请求带
  `X-UI-Proxy-Secret`，由代理注入；缺密钥时 `ui serve` 拒绝启动）；
- 页面通过 `GET /api/state` 的 `perms` 字段决定按钮可用性，权限改了之后刷新
  页面即按新矩阵显示，不缓存权限。

## 展示

- 当前任务/队列/事件序号；
- 当前 checkpoint 与 label；
- 最近 checkpoint / review / revert；
- 会话列表；
- `/api/metrics` 与 `/api/audit/export` 链接；
- **角色矩阵**：角色 × `write/audit/manage` checkbox 表 + 保存按钮；
- **用户表**：`id / role / disabled / display` + 每行「编辑 / 删除」；
- **用户表单**：新增（email 或 preferred_username、角色下拉、显示名、停用）
  与编辑（用户 ID 只读）。

## 写操作

| 表单 | API | 需要的权限 | 落盘 | 审计 |
|---|---|---|---|---|
| 角色矩阵保存 | `PUT /api/admin/roles` | 角色名 `owner` | `.ui/rbac.json` | `rbac/role-update` |
| 新增/编辑用户 | `PUT /api/admin/users` | `manage`；涉及 owner 记录时需 `owner` | `.ui/users.json` | `rbac/user-update` |
| 删除用户 | `DELETE /api/admin/users?id=...` | `manage`；删除 owner 需 `owner` | `.ui/users.json` | `rbac/user-update` |

- 保存后页面重新拉取 `/api/admin/rbac` 与 `/api/admin/users`，并显示
  `journalSeq`（审计事件序号）；失败时错误卡片显示服务端 `error` 原文；
- 矩阵保存是**整份替换**：页面把当前所有行（含未改动的）一起提交，矩阵不能
  为空，至少保留 `owner`；
- 若某角色仍被用户表引用，删它的写请求会被 400 拒绝（先改用户，避免留下指向
  未知角色的映射）；
- `PUT /api/admin/users` 的 `role` 省略时只更新 `disabled/display`（角色沿用
  现有记录）；`role:""` 是 M6 旧行为的删除，等价于 `DELETE`；
- 未知字段、非布尔 `disabled`、超过 128 字符或含控制字符的 `display` 一律 400。

## 用户存储

`.ui/users.json` 的结构（对象包一层 `users` 数组；`display` 可省略）：

```json
{
  "users": [
    {"id": "alice@example.com", "role": "operator", "disabled": false, "display": "Alice"},
    {"id": "bob@example.com", "role": "viewer", "disabled": true}
  ]
}
```

加载规则：

1. `users.json` 存在 → 它是用户表的**唯一事实**；
2. 不存在 → 回退 M6 旧格式 `.ui/rbac.json` 的 `users` 映射（只有 id → role，
   没有停用/显示名）；页面 `source` 显示 `.ui/rbac.json#users`；
3. 存在但损坏（非 JSON、未知字段、未知角色、重复 id、`disabled` 非布尔、
   `role` 缺失）→ 启动直接报错退出；运行期若内存态是 `Invalid`，所有 `/api/*`
   返回 503（fail closed）；
4. 第一次通过 API 写用户表时，旧映射会整体迁进 `users.json`；之后
   `rbac.json` 的 `users` 只是"文件被删掉时的兜底"，不再是事实来源；
5. 删除用户写回空数组（文件保留），不会靠删文件来表达"空表"。

生效顺序：代理密钥/来源 → token → `users.json` 覆盖角色 → 权限矩阵。
`disabled: true` 的用户**不是**降成 viewer：所有请求（含页面、`/api/*`）直接
403，文案写明账号被停用。401 会误导代理层去重新登录，403 才是正确的语义。

## 权限边界

- `/admin` 页面本身只展示，真正判定在 `/api/*`；
- `GET /api/admin/rbac`、`GET /api/admin/users`：需要 `audit` 或 `manage`；
- `PUT /api/admin/roles`：只有角色名 `owner`；
- `PUT/DELETE /api/admin/users`：需要 `manage`；授予/修改/删除 `owner` 记录
  只有 `owner` 能做（`manage` 不够）；
- 所有 `/api/admin/*` 都要求受信代理 actor：无 actor 一律 401（本地模式也不
  放行）；
- 矩阵来自 `.ui/rbac.json`；文件缺失用内置矩阵，损坏拒绝启动；运行期
  `Invalid` 时管理面 503；
- 写操作顺序：校验 → 原子落盘 → 更新内存 → 写审计；响应带 `journalSeq`；
  盘上失败时内存保持旧状态，不会"响应说成功、重启就回退"；
- 代理密钥不会出现在事件、journal、`rbac.json`、`users.json` 或响应正文里。

## 审计

| 事件 | 触发 | 关键字段 |
|---|---|---|
| `rbac/role-update` | `PUT /api/admin/roles` 成功 | `actor`、`roles`（完整新矩阵） |
| `rbac/user-update` | `PUT/DELETE /api/admin/users` 成功 | `actor`、`id`、`role`、`action`（`add/update/disable/enable/revoke`）、`disabled`、可选 `display` |

审计写**当前** session journal（`default` 或 `.ui/sessions/<name>.jsonl`），不是
固定写 default。

## 测试

- `admin_page_is_served`（页面含角色矩阵 checkbox、用户表单、无外部 CDN）；
- `viewer_cannot_read_admin_rbac_but_auditor_can`；
- `operator_cannot_change_roles_but_owner_can_and_audit_lands`；
- `rbac_user_mapping_overrides_header_roles`；
- `corrupt_rbac_json_fails_closed`；
- `rbac_role_update_validates_names_and_booleans`；
- `rbac_api_never_writes_proxy_secret`；
- `rbac_audit_goes_to_the_current_session_journal`；
- `users_json_overrides_roles_and_disabled_is_403`；
- `corrupt_users_json_fails_closed`；
- `admin_users_api_requires_audit_to_read_and_manage_to_write`；
- `only_owner_can_grant_or_revoke_owner`；
- `users_json_migrates_legacy_mapping_and_audits_changes`；
- `role_matrix_cannot_drop_role_referenced_by_users_json`。

OIDC 适配层见 [docs/51](51-oidc.md)；身份边界与部署拓扑见
[docs/41](41-enterprise-m1.md)。
