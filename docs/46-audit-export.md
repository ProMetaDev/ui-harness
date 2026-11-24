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
  "since": 0,
  "limit": 0,
  "events": [ ... ],
  "sha256": "...",
  "hmacAlg": "HMAC-SHA256",
  "keyId": "k1",
  "hmac": "..."
}
```

- `events` 是 journal 原始事件（已按落账门脱敏）；
- `sha256` 是事件 canonical JSON 的哈希，用于离线校验；
- `hmac` 在设置了 `UI_AUDIT_HMAC_KEY` 时出现，否则为 `null`；
- `keyId` 来自 `UI_AUDIT_KEY_ID`，只用于轮换选择，不是秘密；
- journal 损坏时返回 500，拒绝导出。

## HMAC

```text
signed = "ui-audit-v1\n"
       + "session=<session>\n"
       + "total=<total>\n"
       + "count=<count>\n"
       + "since=<since>\n"
       + "limit=<limit>\n"
       + "keyId=<keyId>\n"
       + "sha256=<sha256>\n"
       + <events canonical JSON，按行 join("\n")>

hmac = hex(HMAC-SHA256(UI_AUDIT_HMAC_KEY, signed))
```

- 签名覆盖范围/截断参数，换 range 不能复用旧签名；
- `exportedAt` 不参与签名，所以同一批事件重复导出签名稳定；
- 这是**对称 HMAC**，不是不可抵赖签名；密钥泄露即可伪造。

## 权限

- 受 `/api/*` 的 token / 受信代理约束；
- `--trusted-proxy` 模式下（每个请求须带 `X-UI-Proxy-Secret`，否则 401）：
  - `auditor` / `admin` / `owner` 可导出；
  - `viewer` / `operator` 返回 403。

## 未实现

- WORM/对象存储；
- 非对称签名（当前是对称 HMAC，不具备公开可验证性）；
- 时间范围导出（当前只有 seq 范围）；
- 审计员“只看脱敏正文”的可配置视图；
- 导出审计事件本身（当前只导出，不额外落一条 export 事件）。

## 测试

- `audit_export_is_verifiable_and_role_checked`
- `audit_export_hmac_is_stable_and_optional`
- 验证 count、sha256 长度、viewer 403、无密钥时 `hmac=null`、有密钥时签名稳定。
