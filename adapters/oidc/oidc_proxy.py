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
- userinfo 必须返回非空 `sub`；`X-UI-User` 默认取 `sub`（`OIDC_IDENTITY=sub`），
  `OIDC_IDENTITY=email` 只在 `email_verified is True` 时可用；
- `OIDC_AUD_MODE` 控制 `aud` 校验：`strict`（默认，缺 `aud` 即拒绝）、
  `if-present`（有 `aud` 才校验）、`off`（不校验）；
- `OIDC_ROLES_CLAIM` 可以是普通声明名，也可以是 `realm_access.roles` 这种点路径；
- userinfo 401/403 → 401；网络错误/5xx/坏 JSON → 502（fail closed，不猜测身份）；
- `/api/events`（及 `/api/stream`）用独立的 `OIDC_SSE_TIMEOUT`，普通请求用
  `OIDC_TIMEOUT`，SSE 不会被 5 秒读超时提前断开；
- 非回环 upstream 只允许 `https://`；回环地址允许 `http://`；
- 上游 `ui serve` 的状态码、响应头、正文（含 SSE）原样透传。
"""

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from http.client import HTTPConnection, HTTPSConnection, HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MAX_BODY = 1024 * 1024
MAX_USERINFO_BYTES = 256 * 1024
DEFAULT_TIMEOUT = 5.0
DEFAULT_SSE_TIMEOUT = 300.0
DEFAULT_ROLES_CLAIM = "roles"
DEFAULT_AUD_MODE = "strict"
DEFAULT_IDENTITY = "sub"
AUD_MODES = ("strict", "if-present", "off")
IDENTITY_MODES = ("sub", "email")
# SSE 长连接：普通请求的 5s 读超时会把它们提前掐断，必须单独放宽。
SSE_PATHS = frozenset(["/api/events", "/api/stream"])

USER_HEADER = "X-UI-User"
EMAIL_HEADER = "X-UI-Email"
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
    sse_timeout: float = DEFAULT_SSE_TIMEOUT
    aud_mode: str = DEFAULT_AUD_MODE
    identity: str = DEFAULT_IDENTITY

    @property
    def upstream_scheme(self):
        return urllib.parse.urlsplit(self.upstream_url).scheme

    @property
    def upstream_host(self):
        return urllib.parse.urlsplit(self.upstream_url).hostname

    @property
    def upstream_port(self):
        parsed = urllib.parse.urlsplit(self.upstream_url)
        return parsed.port or (443 if parsed.scheme == "https" else 80)

    @property
    def upstream_authority(self):
        """Host 头用的 authority：IPv6 加方括号，默认端口不重复写。"""
        host = self.upstream_host or ""
        if ":" in host and not host.startswith("["):
            host = "[{}]".format(host)
        default_port = 443 if self.upstream_scheme == "https" else 80
        if self.upstream_port == default_port:
            return host
        return "{}:{}".format(host, self.upstream_port)


def _loopback(host):
    return (host or "") in LOOPBACK_HOSTS


def is_sse_path(path):
    """SSE 端点的路径判断；`/api/stream` 为旧/兼容端点，存在即生效。"""
    return (path or "").rstrip("/") in SSE_PATHS


def load_config(env):
    """从环境变量读配置。所有"弱配置"都在启动时拒绝，不在请求时降级。"""
    userinfo_url = (env.get("OIDC_USERINFO_URL") or "").strip()
    client_id = (env.get("OIDC_CLIENT_ID") or "").strip()
    roles_claim = (env.get("OIDC_ROLES_CLAIM") or "").strip() or DEFAULT_ROLES_CLAIM
    upstream_url = (env.get("UI_UPSTREAM_URL") or "http://127.0.0.1:7788").strip()
    proxy_secret = env.get("UI_PROXY_SECRET") or ""
    listen = (env.get("OIDC_LISTEN") or "127.0.0.1:7789").strip()
    # OIDC_TIMEOUT 是普通请求 canonical 名称；OIDC_USERINFO_TIMEOUT 保留为旧名。
    timeout_raw = (
        env.get("OIDC_TIMEOUT") or env.get("OIDC_USERINFO_TIMEOUT") or ""
    ).strip()
    sse_timeout_raw = (env.get("OIDC_SSE_TIMEOUT") or "").strip()
    aud_mode = (env.get("OIDC_AUD_MODE") or DEFAULT_AUD_MODE).strip().lower()
    identity = (env.get("OIDC_IDENTITY") or DEFAULT_IDENTITY).strip().lower()

    if not userinfo_url:
        raise ConfigError("缺少 OIDC_USERINFO_URL")
    if not proxy_secret.strip():
        raise ConfigError("缺少 UI_PROXY_SECRET（必须与 ui serve --trusted-proxy-secret 相同）")
    if not userinfo_url.lower().startswith(("https://", "http://")):
        raise ConfigError("OIDC_USERINFO_URL 必须是 http(s) URL")
    if aud_mode not in AUD_MODES:
        raise ConfigError("OIDC_AUD_MODE 只能是 strict / if-present / off")
    if identity not in IDENTITY_MODES:
        raise ConfigError("OIDC_IDENTITY 只能是 sub / email")
    parse_listen(listen)
    parse_upstream(upstream_url)

    timeout = _parse_seconds("OIDC_TIMEOUT", timeout_raw, DEFAULT_TIMEOUT, 0.1, 60)
    sse_timeout = _parse_seconds(
        "OIDC_SSE_TIMEOUT", sse_timeout_raw, DEFAULT_SSE_TIMEOUT, 0.1, 3600
    )

    return Config(
        userinfo_url=userinfo_url,
        client_id=client_id,
        roles_claim=roles_claim,
        upstream_url=upstream_url,
        proxy_secret=proxy_secret.strip(),
        listen=listen,
        timeout=timeout,
        sse_timeout=sse_timeout,
        aud_mode=aud_mode,
        identity=identity,
    )


def _parse_seconds(name, raw, default, low, high):
    if not raw:
        return default
    try:
        value = float(raw)
    except ValueError:
        raise ConfigError("{} 必须是数字".format(name))
    if not (low <= value <= high):
        raise ConfigError("{} 必须在 {}..{} 秒之间".format(name, low, high))
    return value


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
    if parsed.scheme not in ("http", "https"):
        raise ConfigError("UI_UPSTREAM_URL 必须是 http(s)://")
    if not parsed.hostname:
        raise ConfigError("UI_UPSTREAM_URL 缺少主机名")
    try:
        parsed.port  # 端口非法（非数字/越界）时 urlsplit 会抛 ValueError
    except ValueError:
        raise ConfigError("UI_UPSTREAM_URL 的端口非法")
    # 明文只会出现在回环地址（本机 ui serve）。非回环必须 HTTPS，
    # 否则 Bearer token / 代理密钥会以明文穿过网络。
    if parsed.scheme == "http" and not _loopback(parsed.hostname):
        raise ConfigError(
            "非回环 UI_UPSTREAM_URL 必须使用 https://；"
            "http:// 只允许 127.0.0.1 / localhost / ::1"
        )
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

    check_audience(config, info)
    return info


def _audience_values(info):
    """把 aud 归一成字符串列表；形状不认识时返回 None（调用方 fail closed）。"""
    raw = info.get("aud")
    if isinstance(raw, str):
        value = raw.strip()
        return [value] if value else []
    if isinstance(raw, list):
        return [v.strip() for v in raw if isinstance(v, str) and v.strip()]
    return None


def check_audience(config, info):
    """按 OIDC_AUD_MODE 校验 aud。缺省 strict：缺 aud 也拒绝。"""
    if not config.client_id or config.aud_mode == "off":
        return

    raw = info.get("aud")
    missing = raw is None or raw == "" or raw == []
    if missing:
        if config.aud_mode == "strict":
            raise UserinfoError("userinfo 缺少 aud（OIDC_AUD_MODE=strict）", status=401)
        return  # if-present：IdP 的 /userinfo 不返回 aud 时放行

    allowed = _audience_values(info)
    if allowed is None:
        raise UserinfoError("userinfo 的 aud 形状不受支持", status=401)
    if not allowed:
        # 例如 [1, 2]：有 aud 字段但没有可用的字符串 audience。
        if config.aud_mode == "strict":
            raise UserinfoError("userinfo 缺少可用的 aud", status=401)
        return
    if config.client_id not in allowed:
        raise UserinfoError("token 的 aud 不包含 OIDC_CLIENT_ID", status=401)


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


def identity_from_userinfo(config, info):
    """解析 `X-UI-User`，返回 (user, email_for_display)。

    - 默认 `OIDC_IDENTITY=sub`：主键是 `sub`；`email` 只在
      `email_verified is True` 时作为展示头，未验证的邮箱不进任何身份头；
    - `OIDC_IDENTITY=email`：只有 `email_verified is True` 的邮箱才可作为主键，
      否则 401（避免把可变的未验证邮箱当成身份）。
    """
    sub = _clean_header_value(info.get("sub"))
    if not sub:
        raise UserinfoError("userinfo 缺少 sub", status=401)
    email = _clean_header_value(info.get("email"))
    verified = info.get("email_verified") is True

    if config.identity == "email":
        if not email:
            raise UserinfoError("OIDC_IDENTITY=email 但 userinfo 没有 email", status=401)
        if not verified:
            raise UserinfoError(
                "OIDC_IDENTITY=email 但 email_verified 不是 true", status=401
            )
        return email, email
    return sub, (email if verified else "")


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
            user, email = identity_from_userinfo(self.server.config, info)
        except UserinfoError as exc:
            if exc.status in (401, 403):
                return self._deny(401, "token 无效或已过期")
            return self._deny(502, "userinfo 不可用（fail closed）")

        roles = roles_from_claim(claim_value(info, self.server.config.roles_claim))
        body = self._read_body()
        if body is None:
            return  # 已回 411/413/400
        headers = self._forward_headers(user, roles, email)
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

    def _forward_headers(self, user, roles, email):
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
            # 客户端 Host 一律不转发：Host 由 _proxy 固定成 upstream authority，
            # 既避免 Host 注入，也避免大小写不同的 Host 造成重复头。
            if low == "host":
                continue
            if low in HOP_BY_HOP or low == "content-length":
                continue
            # 统一小写键：HTTP 头大小写不敏感，去重/覆盖只按小写判断。
            if low in out:
                # 多个 Cookie 头按 RFC 6265 用 "; " 连接，其余按 ", "。
                separator = "; " if low == "cookie" else ", "
                out[low] = out[low] + separator + value
            else:
                out[low] = value
        out[USER_HEADER.lower()] = user
        if email:
            out[EMAIL_HEADER.lower()] = email
        if roles:
            out[ROLES_HEADER.lower()] = ",".join(roles)
        out[SECRET_HEADER.lower()] = config.proxy_secret
        # 不注入 X-UI-UserId：ui serve 优先读它，注入 sub 会让用户 id 变成 sub，
        # 与 `.ui/users.json` 里的 email 键对不上。映射键统一用 X-UI-User。
        return out

    def _proxy(self, config, headers, body):
        connection_cls = (
            HTTPSConnection if config.upstream_scheme == "https" else HTTPConnection
        )
        conn = connection_cls(
            config.upstream_host, config.upstream_port, timeout=config.timeout
        )
        read_timeout = (
            config.sse_timeout if is_sse_path(self.safe_path) else config.timeout
        )
        try:
            # Host 固定为 upstream 的 authority（不采信客户端 Host）。
            # 浏览器同源写请求靠 Sec-Fetch-Site 通过 CSRF 检查；没有该头的
            # 旧客户端会因 Origin 与 upstream Host 不一致而被 ui serve 拒绝（fail closed）。
            conn.putrequest(self.command, self.path, skip_host=True, skip_accept_encoding=True)
            conn.putheader("host", config.upstream_authority)
            for name, value in headers.items():
                conn.putheader(name, value)
            conn.putheader("content-length", str(len(body)))
            conn.putheader("connection", "close")
            conn.endheaders()
            if body:
                conn.send(body)
            # 连接/发送用普通超时；读响应用路径对应的超时（SSE 单独放宽）。
            if conn.sock is not None:
                conn.sock.settimeout(read_timeout)
        except (OSError, HTTPException):
            conn.close()
            self._deny(502, "上游 ui serve 不可用")
            return

        try:
            resp = conn.getresponse()
        except (OSError, HTTPException):
            conn.close()
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
                    # read1：有多少先读多少，SSE 心跳/事件不必等满 8 KiB。
                    chunk = resp.read1(8192)
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
    elif config.aud_mode == "off":
        sys.stderr.write("警告: OIDC_AUD_MODE=off，userinfo 的 aud 不校验\n")
    if config.identity == "email":
        sys.stderr.write(
            "警告: OIDC_IDENTITY=email，仅接受 email_verified=true；生产建议用 sub\n"
        )
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
