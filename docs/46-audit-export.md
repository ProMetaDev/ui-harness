# 审计导出 `/api/audit/export`

> M5 基础：把当前 session 的 journal 导出为可校验 JSON；不引入外部依赖。

## 范围

```text
GET /api/audit/export?since=<seq>&limit=<n>
```

- `since`：只导出 seq ≥ 该值的事件；
- `limit`：最多导出 n 条（1–10000）；
- 响应同时给 `total` 与 `count`，便于知道是否被截断。

## 响应

```json
{
  "version": 1,
  "session": "default",
  "exportedAt": 1790000000000,
  "total": 12,
  "count": 12,
  "events": [ ... ],
  "sha256": "..."
}
```

- `events` 是 journal 原始事件（已按落账门脱敏）；
- `sha256` 是事件 canonical JSON 的哈希，用于离线校验；
- journal 损坏时返回 500，拒绝导出。

## 权限

- 受 `/api/*` 的 token / 受信代理约束；
- `--trusted-proxy` 模式下：
  - `auditor` / `admin` / `owner` 可导出；
  - `viewer` / `operator` 返回 403。

## 未实现

- WORM/对象存储；
- 签名（只有 sha256，不是不可抵赖签名）；
- 时间范围导出（当前只有 seq 范围）；
- 审计员“只看脱敏正文”的可配置视图；
- 导出审计事件本身（当前只导出，不额外落一条 export 事件）。

## 测试

- `audit_export_is_verifiable_and_role_checked`
- 验证 count、sha256 长度、viewer 403。
