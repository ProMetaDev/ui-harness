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
- `GET /api/metrics` 返回 200、`text/plain`、`ui_up 1`。
