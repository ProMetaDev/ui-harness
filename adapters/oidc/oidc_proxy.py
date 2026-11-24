#!/usr/bin/env python3
"""OIDC userinfo 适配层（可选 L2）。

它**不是** OIDC 客户端，也不做 authorization code flow：

- token 由现有 OIDC 客户端（oauth2-proxy、企业网关、SPA 的 OIDC 库）获取；
- 本进程只做 userinfo 校验与身份头注入，然后把请求原样转给 `ui serve`；
- 只监听回环地址，不打印 token / Authorization / 响应里的身份字段。

链路：

    Browser --(HTTPS/OIDC cookie 由网关处理)--> oauth2-proxy / 网关
       --(Authorization: Bearer <access_token>)--> 本适配层
       --(X-UI-User / X-UI-Roles / X-UI-Proxy-Secret)--> ui serve --trusted-proxy

强制约束：

- 客户端自带的 `X-UI-*` 一律先剥离再注入；`Authorization` 不向上游转发；
- userinfo 必须返回非空 `sub`；`email`/`preferred_username` 作为 `X-UI-User`，
  两者都没有时退到 `sub`（`sub` 仍会先被校验）；
- `OIDC_ROLES_CLAIM` 可以是普通声明名，也可以是 `realm_access.roles` 这种点路径；
- userinfo 401/403 → 401；网络错误/5xx/坏 JSON → 502（fail closed，不猜测身份）；
- 上游 `ui serve` 的状态码、响应头、正文（含 SSE）原样透传。
"""

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from http.client import HTTPConnection, HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MAX_BODY = 1024 * 1024
MAX_USERINFO_BYTES = 256 * 1024
DEFAULT_TIMEOUT = 5.0
DEFAULT_ROLES_CLAIM = "roles"

USER_HEADER = "X-UI-User"
ROLES_HEADER = "X-UI-Roles"
SECRET_HEADER = "X-UI-Proxy-Secret"

# RFC 7230 hop-by-hop：既不能转发，也不能把上游的转回来。
HOP_BY_HOP = frozenset(
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
    ]
)

LOOPBACK_HOSTS = frozenset(["127.0.0.1", "localhost", "::1"])


class ConfigError(Exception):
    """启动配置不成立：宁可起不来，也不带着弱配置对外服务。"""


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """userinfo 不跟重定向：避免 Authorization 被带到另一个地址。"""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


_USERINFO_OPENER = urllib.request.build_opener(_NoRedirect)


class UserinfoError(Exception):
    def __init__(self, message, status=None):
        super().__init__(message)
        self.status = status


@dataclass(frozen=True)
class Config:
    userinfo_url: str
    client_id: str
    roles_claim: str
    upstream_url: str
    proxy_secret: str
    listen: str
    timeout: float = DEFAULT_TIMEOUT

    @property
    def upstream_host(self):
        return urllib.parse.urlsplit(self.upstream_url).hostname

    @property
    def upstream_port(self):
        return urllib.parse.urlsplit(self.upstream_url).port or 80


def _loopback(host):
    return (host or "") in LOOPBACK_HOSTS


def load_config(env):
    """从环境变量读配置。所有"弱配置"都在启动时拒绝，不在请求时降级。"""
    userinfo_url = (env.get("OIDC_USERINFO_URL") or "").strip()
    client_id = (env.get("OIDC_CLIENT_ID") or "").strip()
    roles_claim = (env.get("OIDC_ROLES_CLAIM") or "").strip() or DEFAULT_ROLES_CLAIM
    upstream_url = (env.get("UI_UPSTREAM_URL") or "http://127.0.0.1:7788").strip()
    proxy_secret = env.get("UI_PROXY_SECRET") or ""
    listen = (env.get("OIDC_LISTEN") or "127.0.0.1:7789").strip()
    timeout_raw = (env.get("OIDC_USERINFO_TIMEOUT") or "").strip()

    if not userinfo_url:
        raise ConfigError("缺少 OIDC_USERINFO_URL")
    if not proxy_secret.strip():
        raise ConfigError("缺少 UI_PROXY_SECRET（必须与 ui serve --trusted-proxy-secret 相同）")
    if not userinfo_url.lower().startswith(("https://", "http://")):
        raise ConfigError("OIDC_USERINFO_URL 必须是 http(s) URL")
    parse_listen(listen)
    parse_upstream(upstream_url)

    try:
        timeout = float(timeout_raw) if timeout_raw else DEFAULT_TIMEOUT
    except ValueError:
        raise ConfigError("OIDC_USERINFO_TIMEOUT 必须是数字")
    if not (0.1 <= timeout <= 60):
        raise ConfigError("OIDC_USERINFO_TIMEOUT 必须在 0.1..60 秒之间")

    return Config(
        userinfo_url=userinfo_url,
        client_id=client_id,
        roles_claim=roles_claim,
        upstream_url=upstream_url,
        proxy_secret=proxy_secret.strip(),
        listen=listen,
        timeout=timeout,
    )


def parse_listen(value):
    host, sep, port = value.rpartition(":")
    if not sep or not port.isdigit():
        raise ConfigError("OIDC_LISTEN 必须是 host:port，例如 127.0.0.1:7789")
    host = host.strip("[]")
    if not _loopback(host):
        raise ConfigError("OIDC_LISTEN 只允许回环地址（127.0.0.1 / localhost / ::1）")
    port = int(port)
    if not (0 <= port <= 65535):
        raise ConfigError("OIDC_LISTEN 端口越界")
    return host, port


def parse_upstream(value):
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme != "http":
        raise ConfigError("UI_UPSTREAM_URL 必须是 http://127.0.0.1:<port>（本机 ui serve）")
    if not _loopback(parsed.hostname):
        raise ConfigError("UI_UPSTREAM_URL 只允许回环地址，避免把代理密钥发到别处")
    return parsed


def fetch_userinfo(config, token):
    """调 userinfo 并做最小校验。异常信息里**不带** token。"""
    req = urllib.request.Request(
        config.userinfo_url,
        headers={
            "Authorization": "Bearer " + token,
            "Accept": "application/json",
        },
    )
    try:
        with _USERINFO_OPENER.open(req, timeout=config.timeout) as resp:
            status = resp.getcode()
            body = resp.read(MAX_USERINFO_BYTES)
    except urllib.error.HTTPError as exc:
        raise UserinfoError("userinfo 拒绝该 token", status=exc.code)
    except (urllib.error.URLError, OSError) as exc:
        reason = getattr(exc, "reason", exc)
        raise UserinfoError("userinfo 不可达: {}".format(reason))

    if status != 200:
        raise UserinfoError("userinfo 返回 {}".format(status), status=status)

    try:
        info = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        raise UserinfoError("userinfo 响应不是合法 JSON")
    if not isinstance(info, dict):
        raise UserinfoError("userinfo 响应必须是 JSON 对象")

    sub = info.get("sub")
    if not isinstance(sub, str) or not sub.strip():
        raise UserinfoError("userinfo 缺少 sub", status=401)

    if config.client_id:
        aud = info.get("aud")
        if aud is not None:
            allowed = [aud] if isinstance(aud, str) else aud if isinstance(aud, list) else []
            if config.client_id not in allowed:
                raise UserinfoError("token 的 aud 不包含 OIDC_CLIENT_ID", status=401)
    return info


def claim_value(info, path):
    """按点路径取声明；`realm_access.roles` 这种嵌套也能取到。"""
    current = info
    for part in path.split("."):
        if not isinstance(current, dict) or part not in current:
            return None
        current = current[part]
    return current


def _valid_role(role):
    if not role or len(role) > 64:
        return False
    return all(("a" <= c <= "z") or ("0" <= c <= "9") or c in "_-." for c in role)


def roles_from_claim(value):
    """把 roles 声明归一成 ui serve 认的小写 CSV；不认识的一律丢弃。"""
    if isinstance(value, str):
        items = value.split(",")
    elif isinstance(value, list):
        items = [v for v in value if isinstance(v, str)]
    else:
        items = []
    out = []
    for raw in items:
        role = raw.strip().lower()
        if _valid_role(role) and role not in out:
            out.append(role)
    return out[:32]


def _clean_header_value(value, limit=256):
    if not isinstance(value, str):
        return ""
    cleaned = value.strip()
    if len(cleaned) > limit:
        return ""
    if any(ord(c) < 32 for c in cleaned) or "\x7f" in cleaned:
        return ""
    return cleaned


def identity_from_userinfo(info):
    """`X-UI-User`：email → preferred_username → sub（sub 已在 fetch 里校验过）。"""
    for key in ("email", "preferred_username"):
        cleaned = _clean_header_value(info.get(key))
        if cleaned:
            return cleaned
    return _clean_header_value(info.get("sub"))


class Userinfo:
    """测试可替换的取用户信息入口。"""

    @staticmethod
    def fetch(config, token):
        return fetch_userinfo(config, token)


class ProxyServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address, config):
        super().__init__(address, ProxyHandler)
        self.config = config


class ProxyHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "ui-oidc-proxy"
    sys_version = ""

    # ── 日志：方法 + 去 query 的路径 + 状态码；不打印头 / token ──
    def log_request(self, code="-", size="-"):
        path = getattr(self, "safe_path", "/")
        sys.stderr.write("{} {} {}\n".format(self.command, path, code))
        sys.stderr.flush()

    def log_error(self, fmt, *args):
        # 默认实现会把请求行（含 query 里的 t= token）打出来，这里一律不写。
        pass

    def do_GET(self):
        self._handle()

    def do_HEAD(self):
        self._handle()

    def do_POST(self):
        self._handle()

    def do_PUT(self):
        self._handle()

    def do_DELETE(self):
        self._handle()

    def do_OPTIONS(self):
        self._handle()

    # ── 请求处理 ──
    def _handle(self):
        self.safe_path = urllib.parse.urlsplit(self.path).path
        token = self._bearer()
        if not token:
            return self._deny(401, "缺少 Authorization: Bearer token")

        try:
            info = Userinfo.fetch(self.server.config, token)
        except UserinfoError as exc:
            if exc.status in (401, 403):
                return self._deny(401, "token 无效或已过期")
            return self._deny(502, "userinfo 不可用（fail closed）")

        user = identity_from_userinfo(info)
        if not user:
            return self._deny(401, "userinfo 没有可用身份字段")

        roles = roles_from_claim(claim_value(info, self.server.config.roles_claim))
        body = self._read_body()
        if body is None:
            return  # 已回 411/413/400
        headers = self._forward_headers(user, roles)
        self._proxy(self.server.config, headers, body)

    def _bearer(self):
        raw = self.headers.get("Authorization", "")
        if not raw.lower().startswith("bearer "):
            return ""
        return raw[7:].strip()

    def _read_body(self):
        if "transfer-encoding" in self.headers:
            # ui serve 只认 Content-Length；chunked 请求在这里拒绝，避免请求走私。
            self._deny(411, "只接受带 Content-Length 的请求体")
            return None
        raw = self.headers.get("Content-Length")
        if raw is None:
            if self.command in ("POST", "PUT", "PATCH"):
                self._deny(411, "缺少 Content-Length")
                return None
            return b""
        try:
            length = int(raw)
        except ValueError:
            self._deny(400, "Content-Length 非法")
            return None
        if length < 0:
            self._deny(400, "Content-Length 非法")
            return None
        if length > MAX_BODY:
            self._deny(413, "请求体过大")
            return None
        body = b""
        while len(body) < length:
            chunk = self.rfile.read(length - len(body))
            if not chunk:
                self._deny(400, "请求体不完整")
                return None
            body += chunk
        return body

    def _forward_headers(self, user, roles):
        config = self.server.config
        out = {}
        for name, value in self.headers.items():
            low = name.lower()
            # 客户端自带的 X-UI-*（含伪造的 Proxy-Secret）先全部剥离。
            if low.startswith("x-ui-"):
                continue
            # token 不向上游转发：ui serve 只认共享密钥 + 身份头。
            if low == "authorization":
                continue
            if low in HOP_BY_HOP or low == "content-length":
                continue
            if name in out:
                out[name] = out[name] + ", " + value
            else:
                out[name] = value
        out[USER_HEADER] = user
        if roles:
            out[ROLES_HEADER] = ",".join(roles)
        out[SECRET_HEADER] = config.proxy_secret
        # 不注入 X-UI-UserId：ui serve 优先读它，注入 sub 会让用户 id 变成 sub，
        # 与 `.ui/users.json` 里的 email 键对不上。映射键统一用 X-UI-User。
        return out

    def _proxy(self, config, headers, body):
        conn = HTTPConnection(config.upstream_host, config.upstream_port, timeout=config.timeout)
        try:
            # Host 原样转发（客户端视角的 Host）：ui serve 的 CSRF 检查按
            # Origin == http://<Host> 比对，改成上游端口会把浏览器的写请求判成跨源。
            host = headers.get("Host") or "{}:{}".format(config.upstream_host, config.upstream_port)
            conn.putrequest(self.command, self.path, skip_host=True, skip_accept_encoding=True)
            conn.putheader("Host", host)
            for name, value in headers.items():
                if name.lower() == "host":
                    continue
                conn.putheader(name, value)
            conn.putheader("Content-Length", str(len(body)))
            conn.putheader("Connection", "close")
            conn.endheaders()
            if body:
                conn.send(body)
        except (OSError, HTTPException):
            self._deny(502, "上游 ui serve 不可用")
            return

        try:
            resp = conn.getresponse()
        except (OSError, HTTPException):
            self._deny(502, "上游 ui serve 无响应")
            return

        try:
            self.send_response(resp.status)
            for name, value in resp.getheaders():
                low = name.lower()
                if low in HOP_BY_HOP or low == "content-length":
                    continue
                self.send_header(name, value)
            # 不写 Content-Length：正文按连接关闭界定，SSE 也能持续流出。
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                while True:
                    chunk = resp.read(8192)
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    self.wfile.flush()
        except (OSError, HTTPException):
            pass
        finally:
            self.close_connection = True
            conn.close()

    def _deny(self, status, message):
        body = json.dumps(
            {"ok": False, "error": message}, ensure_ascii=False
        ).encode("utf-8")
        try:
            self.send_response(status)
            if status == 401:
                self.send_header("WWW-Authenticate", "Bearer")
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
                self.wfile.flush()
        except OSError:
            pass
        finally:
            self.close_connection = True


def main():
    try:
        config = load_config(os.environ)
    except ConfigError as exc:
        sys.stderr.write("配置错误: {}\n".format(exc))
        return 2
    if not config.client_id:
        sys.stderr.write("警告: 未设置 OIDC_CLIENT_ID，跳过 aud 校验\n")
    host, port = parse_listen(config.listen)
    server = ProxyServer((host, port), config)
    bound_host, bound_port = server.server_address[:2]
    sys.stderr.write(
        "ui-oidc-proxy listening on http://{}:{} -> {}\n".format(
            bound_host, bound_port, config.upstream_url
        )
    )
    sys.stderr.flush()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
