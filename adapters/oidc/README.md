# OIDC userinfo 适配层（可选 L2）

`oidc_proxy.py` 是 `ui serve --trusted-proxy` 前面的翻译层：

```text
Authorization: Bearer <token>  ->  userinfo  ->  X-UI-User / X-UI-Roles
                                              +  X-UI-Proxy-Secret
```

它**不做** authorization code flow、不签发 token、不验 JWT 签名；token 由现有
OIDC 客户端（oauth2-proxy / 企业网关 / 前端 OIDC 库）获取。它只监听回环地址，
不打印 token，剥离客户端自带的 `X-UI-*` 后再注入。

```bash
OIDC_USERINFO_URL=https://idp.example.com/userinfo \
OIDC_CLIENT_ID=ui-enterprise \
OIDC_ROLES_CLAIM=roles \
UI_UPSTREAM_URL=http://127.0.0.1:7788 \
UI_PROXY_SECRET="$PROXY_SECRET" \
OIDC_LISTEN=127.0.0.1:7789 \
python3 adapters/oidc/oidc_proxy.py

python3 adapters/oidc/test_proxy.py   # 本地 mock userinfo / upstream
```

完整边界、环境变量与安全注意见 [docs/51](../../docs/51-oidc.md)。
