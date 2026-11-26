# OIDC userinfo 适配层（可选 L2）

`oidc_proxy.py` 是 `ui serve --trusted-proxy` 前面的翻译层：

```text
Authorization: Bearer <token>  ->  userinfo  ->  X-UI-User / X-UI-Roles
                                              +  X-UI-Proxy-Secret
```

它**不做** authorization code flow、不签发 token、不验 JWT 签名；token 由现有
OIDC 客户端（oauth2-proxy / 企业网关 / 前端 OIDC 库）获取。它只监听回环地址，
不打印 token，剥离客户端自带的 `X-UI-*` 后再注入。

身份主键默认是 `sub`（`OIDC_IDENTITY=sub`）；`email` 只在
`OIDC_IDENTITY=email` 且 userinfo 的 `email_verified is True` 时作为
`X-UI-User`，否则只作为可选的 `X-UI-Email` 展示头（未验证邮箱不注入）。
`OIDC_AUD_MODE` 默认 `strict`：配置了 `OIDC_CLIENT_ID` 时，userinfo 缺 `aud`
也会 401；标准 IdP 的 `/userinfo` 常常不返回 `aud`，生产应改用
introspection/JWT 验签，或显式设 `OIDC_AUD_MODE=if-present`。

```bash
OIDC_USERINFO_URL=https://idp.example.com/userinfo \
OIDC_CLIENT_ID=ui-enterprise \
OIDC_AUD_MODE=strict \
OIDC_IDENTITY=sub \
OIDC_ROLES_CLAIM=roles \
UI_UPSTREAM_URL=http://127.0.0.1:7788 \
UI_PROXY_SECRET="$PROXY_SECRET" \
OIDC_LISTEN=127.0.0.1:7789 \
OIDC_TIMEOUT=5 OIDC_SSE_TIMEOUT=300 \
python3 adapters/oidc/oidc_proxy.py

python3 adapters/oidc/test_proxy.py   # 本地 mock userinfo / upstream
```

完整边界、环境变量与安全注意见 [docs/51](../../docs/51-oidc.md)。
