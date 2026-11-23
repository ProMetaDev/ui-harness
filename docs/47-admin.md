# `/admin` 管理面

> M6 的只读管理面起点；零依赖原生 HTML/JS，不引入前端框架。

## 访问

```text
http://127.0.0.1:7788/admin
```

- 与其它页面一样受 token / 受信代理约束；
- 本地单用户模式显示“本地单用户模式”；
- `--trusted-proxy` 模式显示 `X-UI-UserId` 与 roles。

## 展示

- 当前任务/队列/事件序号；
- 当前 checkpoint 与 label；
- 最近 checkpoint；
- 最近 review/revert；
- 会话列表；
- `/api/metrics` 与 `/api/audit/export` 链接。

## 边界

- 只读；写操作仍在对话页/CLI；
- 没有用户管理/角色编辑；
- 没有策略编辑；
- 没有分页；
- 没有浏览器 e2e 覆盖 `/admin`；
- 依赖 `/api/*` 的权限，不额外绕过。

## 测试

- `admin_page_is_served`
- 内嵌 JS 语法检查
