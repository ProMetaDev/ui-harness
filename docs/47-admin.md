# `/admin` 管理面

> M6 的最小管理面：零依赖原生 HTML/JS，不引入前端框架。
> 页面只读；RBAC 写操作通过 API 完成，且每次变更落当前 session journal。

## 访问

```text
http://127.0.0.1:7788/admin
```

- 与其它页面一样受 token / 受信代理约束；
- 本地单用户模式显示“本地单用户模式”；
- `--trusted-proxy` 模式显示 `X-UI-UserId` 与 roles（该模式要求每个请求带
  `X-UI-Proxy-Secret`，由代理注入；缺密钥时 `ui serve` 拒绝启动）。

## 展示

- 当前任务/队列/事件序号；
- 当前 checkpoint 与 label；
- 最近 checkpoint；
- 最近 review/revert；
- 会话列表；
- `/api/metrics` 与 `/api/audit/export` 链接；
- **RBAC 只读表**：角色 × `write/audit/manage` 矩阵 + 用户映射。
  数据来自 `GET /api/admin/rbac`；没有 `audit`/`manage` 的角色只会看到
  “RBAC 不可读”，不影响其余卡片。

## 权限边界

- `/admin` 页面本身只展示，真正的判定在 `/api/*`；
- `GET /api/admin/rbac`：需要 `audit` 或 `manage`；
- `PUT /api/admin/roles`：只有角色名 `owner` 能改，整份替换或改单个角色；
- `PUT /api/admin/users`：需要 `manage`，body `{"id":"...","role":"..."}`，
  `role` 为空串等于删除；
- `DELETE /api/admin/users?id=...`：需要 `manage`；
- 所有 `/api/admin/*` 都要求受信代理 actor：无 actor 一律 401（本地模式也不放行）；
- 矩阵来自 `.ui/rbac.json`；文件缺失用内置矩阵，损坏则拒绝启动，
  运行期若为 `Invalid` 则管理面 503（fail closed）；
- 用户映射只覆盖角色，不绕过受信代理密钥/token；
- 写操作顺序：校验 → 原子落盘 → 更新内存 → 写审计，响应带 `journalSeq`；
- 页面没有角色编辑表单，也没有分页。

## 审计

| 事件 | 触发 | 关键字段 |
|---|---|---|
| `rbac/role-update` | `PUT /api/admin/roles` 成功 | `actor`、`roles`（完整新矩阵） |
| `rbac/user-update` | `PUT/DELETE /api/admin/users` 成功 | `actor`、`id`、`role`、`action` |

审计写**当前** session journal（`default` 或 `.ui/sessions/<name>.jsonl`），
不是固定写 default；代理密钥不会出现在事件、journal 或 `rbac.json` 里。

## 测试

- `admin_page_is_served`（含 `/api/admin/rbac` 卡片）
- `viewer_cannot_read_admin_rbac_but_auditor_can`
- `operator_cannot_change_roles_but_owner_can_and_audit_lands`
- `rbac_user_mapping_overrides_header_roles`
- `corrupt_rbac_json_fails_closed`
- `rbac_role_update_validates_names_and_booleans`
- `rbac_api_never_writes_proxy_secret`
- `rbac_audit_goes_to_the_current_session_journal`
