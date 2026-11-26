# 可观测性：`/api/metrics`

> 零依赖 Prometheus 文本格式；不引入 Prometheus client crate。

## 指标

```text
ui_up 1
ui_uptime_seconds
ui_events_seq
ui_events_in_memory
ui_tasks_active
ui_tasks_queued
ui_tools_stored
ui_approval_pending
```

## 访问

```bash
curl -H "Cookie: ui_token=..." http://127.0.0.1:7788/api/metrics
```

- 与 `/api/*` 一样受 token/受信代理约束；
- 通过身份校验的任何角色（包括 viewer）都可以读：指标只有进程计数，不含
  提示词、工具输出或路径，因此不要求 `audit`/`manage`；
- 同一原则下 `/api/file` 对 viewer 只读开放，但仍走工作区路径守卫与敏感文件
  策略（`.env*`、私钥、证书返回 403）；`/api/reviews`、`/api/audit/export`
  仍要求 `audit`；
- 不暴露提示词、工具输出或路径；
- 文本格式为 `text/plain; version=0.0.4`。

## 未实现

- histogram/summary；
- 每租户/用户 label；
- OpenTelemetry trace；
- `/metrics` 标准路径（当前是 `/api/metrics`，便于复用鉴权）；
- 告警规则。

## 测试

- `metrics_endpoint_exposes_gauges`
- `viewer_can_read_metrics_and_safe_files_but_not_sensitive_files`
- `GET /api/metrics` 返回 200、`text/plain`、`ui_up 1`；viewer 读同样 200。
