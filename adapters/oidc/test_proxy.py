#!/usr/bin/env python3
"""`adapters/oidc/oidc_proxy.py` 的回归：本地 mock userinfo + mock ui serve。

覆盖：200 / 401 / 502 / 上游状态透传 / 剥离客户端 X-UI-* / 不转发 token /
roles 归一 / 嵌套 claim / 只监听回环 / 不打印 token /
aud 模式（strict / if-present / off）/ 身份主键（sub / email）/
SSE 独立读超时 / 非回环明文 upstream 拒绝 / Host 去重。
"""

import json
import os
import socket
import subprocess
import sys
import threading
import time
import unittest
from contextlib import contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import oidc_proxy  # noqa: E402

SECRET = "test-proxy-secret-not-real"  # allowlist secret: 测试用假值

USERINFO = {
    "good-token": {
        "sub": "sub-1",
        "email": "alice@example.com",
        "email_verified": True,
        "aud": "ui-enterprise",
        "roles": ["operator", "auditor"],
    },
    "nested-token": {
        "sub": "sub-2",
        "preferred_username": "bob",
        "aud": "ui-enterprise",
        "realm_access": {"roles": ["admin"]},
    },
    "nosub-token": {"email": "nobody@example.com", "aud": "ui-enterprise"},
    "string-roles": {
        "sub": "sub-3",
        "email": "carol@example.com",
        "email_verified": True,
        "aud": "ui-enterprise",
        "roles": "operator, Auditor, bad role, owner",
    },
    "aud-token": {
        "sub": "sub-4",
        "email": "dave@example.com",
        "aud": "some-other-client",
        "roles": [],
    },
    "groups-token": {
        "sub": "sub-5",
        "email": "erin@example.com",
        "email_verified": True,
        "aud": "ui-enterprise",
        "groups": ["owner", "viewer", "bad role"],
    },
    "noaud-token": {
        "sub": "sub-6",
        "email": "frank@example.com",
        "email_verified": True,
        "roles": [],
    },
    "unverified-email-token": {
        "sub": "sub-7",
        "email": "mallory@example.com",
        "email_verified": False,
        "aud": "ui-enterprise",
        "roles": [],
    },
    "string-verified-token": {
        "sub": "sub-8",
        "email": "gina@example.com",
        # 字符串 "true" 不是布尔 True：身份主键只接受真正的布尔值。
        "email_verified": "true",
        "aud": "ui-enterprise",
        "roles": [],
    },
}


class QuietHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # 测试里不刷屏
        pass

    def _send(self, status, payload, headers=None):
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)
            self.wfile.flush()
        self.close_connection = True


class UserinfoHandler(QuietHandler):
    def do_GET(self):
        auth = self.headers.get("Authorization", "")
        token = auth[7:].strip() if auth.lower().startswith("bearer ") else ""
        if self.path.split("?")[0] != "/userinfo":
            return self._send(404, {"error": "not found"})
        if token == "bad-token":
            return self._send(401, {"error": "invalid token"})
        if token == "boom-token":
            return self._send(500, {"error": "idp down"})
        if token not in USERINFO:
            return self._send(401, {"error": "unknown token"})
        return self._send(200, USERINFO[token])


class UpstreamHandler(QuietHandler):
    seen = []

    def _handle(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        UpstreamHandler.seen.append(
            {
                "method": self.command,
                "path": self.path,
                "headers": dict(self.headers.items()),
                "header_names": [name for name, _ in self.headers.items()],
                "host_count": len(self.headers.get_all("Host") or []),
                "body": body,
            }
        )
        path = self.path.split("?")[0]
        if path == "/echo":
            return self._send(200, {"ok": True, "headers": dict(self.headers.items())})
        if path == "/boom":
            return self._send(500, {"ok": False, "error": "upstream boom"})
        if path == "/slow":
            # 比普通读超时慢：证明普通请求仍受 OIDC_TIMEOUT 约束。
            time.sleep(0.8)
            return self._send(200, {"ok": True})
        if path == "/stream":
            # 普通流式透传（非 SSE 端点超时策略）：两段立即写出。
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Connection", "close")
            self.end_headers()
            for chunk in (b"data: one\n\n", b"data: two\n\n"):
                self.wfile.write(chunk)
                self.wfile.flush()
            self.close_connection = True
            return
        if path in ("/api/events", "/api/stream"):
            # 分块之间 0.6s > 普通 timeout(0.3s)，但在 OIDC_SSE_TIMEOUT 之内。
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Connection", "close")
            self.end_headers()
            chunks = (b"data: one\n\n", b"data: two\n\n", b"data: three\n\n")
            for index, chunk in enumerate(chunks):
                self.wfile.write(chunk)
                self.wfile.flush()
                if index < len(chunks) - 1:
                    time.sleep(0.6)
            self.close_connection = True
            return
        return self._send(404, {"ok": False, "error": "no route"})

    do_GET = _handle
    do_POST = _handle
    do_PUT = _handle
    do_DELETE = _handle


class QuietServer(ThreadingHTTPServer):
    """测试里上游线程被客户端提前关闭时不要往 stderr 刷 traceback。"""

    daemon_threads = True

    def handle_error(self, request, client_address):
        pass


def serve(handler):
    server = QuietServer(("127.0.0.1", 0), handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def header(headers, name):
    for key, value in headers.items():
        if key.lower() == name.lower():
            return value
    return None


def free_port():
    sock = socket.socket()
    try:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]
    finally:
        sock.close()


def make_config(userinfo_url, upstream_url, **overrides):
    values = dict(
        userinfo_url=userinfo_url,
        client_id="ui-enterprise",
        roles_claim="roles",
        upstream_url=upstream_url,
        proxy_secret=SECRET,
        listen="127.0.0.1:0",
        timeout=3.0,
        sse_timeout=3.0,
    )
    values.update(overrides)
    return oidc_proxy.Config(**values)


@contextmanager
def running_proxy(config):
    proxy = oidc_proxy.ProxyServer(("127.0.0.1", 0), config)
    proxy.daemon_threads = True
    threading.Thread(target=proxy.serve_forever, daemon=True).start()
    try:
        yield proxy
    finally:
        proxy.shutdown()
        proxy.server_close()


class ProxyTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.userinfo = serve(UserinfoHandler)
        cls.upstream = serve(UpstreamHandler)
        cls.config = make_config(
            "http://127.0.0.1:{}/userinfo".format(cls.userinfo.server_address[1]),
            "http://127.0.0.1:{}".format(cls.upstream.server_address[1]),
        )
        cls.proxy = oidc_proxy.ProxyServer(("127.0.0.1", 0), cls.config)
        cls.proxy.daemon_threads = True
        threading.Thread(target=cls.proxy.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        for server in (cls.proxy, cls.userinfo, cls.upstream):
            server.shutdown()
            server.server_close()

    def setUp(self):
        UpstreamHandler.seen.clear()

    def call(self, method="GET", path="/echo", headers=None, body=None, proxy=None, timeout=10):
        target = proxy or self.proxy
        conn = HTTPConnection("127.0.0.1", target.server_address[1], timeout=timeout)
        try:
            conn.request(method, path, body=body, headers=headers or {})
            resp = conn.getresponse()
            data = resp.read()
            return resp.status, dict(resp.getheaders()), data
        finally:
            conn.close()

    def custom_proxy(self, **overrides):
        return running_proxy(
            make_config(self.config.userinfo_url, self.config.upstream_url, **overrides)
        )

    def auth(self, token="good-token"):
        return {"Authorization": "Bearer " + token}

    # ── 200：注入身份头、剥离客户端头、不转发 token ──
    def test_valid_token_injects_identity_and_strips_client_headers(self):
        status, _, body = self.call(
            headers=dict(
                self.auth(),
                **{
                    "X-UI-User": "evil@example.com",
                    "X-UI-Email": "evil@example.com",
                    "X-UI-Roles": "owner",
                    "X-UI-Proxy-Secret": "forged-secret",
                    "X-UI-Request-Id": "attacker",
                },
            ),
        )
        self.assertEqual(status, 200, body)
        seen = UpstreamHandler.seen[-1]
        # 默认 OIDC_IDENTITY=sub：主键是 sub；已验证的 email 只作展示。
        self.assertEqual(header(seen["headers"], "X-UI-User"), "sub-1")
        self.assertEqual(header(seen["headers"], "X-UI-Email"), "alice@example.com")
        self.assertEqual(header(seen["headers"], "X-UI-Roles"), "operator,auditor")
        self.assertEqual(header(seen["headers"], "X-UI-Proxy-Secret"), SECRET)
        self.assertIsNone(header(seen["headers"], "Authorization"), "token 不能转发给 ui serve")
        joined = json.dumps(seen["headers"])
        self.assertNotIn("evil@example.com", joined)
        self.assertNotIn("forged-secret", joined)
        self.assertNotIn("attacker", joined)

    def test_duplicate_client_x_ui_headers_are_removed(self):
        conn = HTTPConnection("127.0.0.1", self.proxy.server_address[1], timeout=10)
        try:
            conn.putrequest("GET", "/echo")
            conn.putheader("Host", "127.0.0.1:{}".format(self.proxy.server_address[1]))
            conn.putheader("Authorization", "Bearer good-token")
            conn.putheader("X-UI-User", "evil-1@example.com")
            conn.putheader("X-UI-User", "evil-2@example.com")
            conn.putheader("X-UI-Roles", "owner")
            conn.endheaders()
            resp = conn.getresponse()
            resp.read()
            self.assertEqual(resp.status, 200)
        finally:
            conn.close()
        seen = UpstreamHandler.seen[-1]
        self.assertEqual(header(seen["headers"], "X-UI-User"), "sub-1")
        self.assertEqual(header(seen["headers"], "X-UI-Roles"), "operator,auditor")

    def test_post_body_is_forwarded_and_host_is_upstream(self):
        status, _, _ = self.call(
            method="POST",
            path="/echo",
            headers=dict(self.auth(), **{"Content-Type": "application/json", "Origin": "http://127.0.0.1"}),
            body=b'{"text":"hi"}',
        )
        self.assertEqual(status, 200)
        seen = UpstreamHandler.seen[-1]
        self.assertEqual(seen["body"], b'{"text":"hi"}')
        self.assertEqual(seen["method"], "POST")
        # Host 固定成 upstream authority：不采信客户端 Host，也不会重复。
        self.assertEqual(
            header(seen["headers"], "Host"),
            "127.0.0.1:{}".format(self.upstream.server_address[1]),
        )
        self.assertEqual(seen["host_count"], 1)
        self.assertEqual(seen["path"], "/echo")

    def test_client_host_is_dropped_and_forwarded_keys_are_lowercase(self):
        conn = HTTPConnection("127.0.0.1", self.proxy.server_address[1], timeout=10)
        try:
            conn.putrequest("GET", "/echo", skip_host=True)
            conn.putheader("hOsT", "evil.example")
            conn.putheader("AuThOrIzAtIoN", "Bearer good-token")
            conn.endheaders()
            resp = conn.getresponse()
            resp.read()
            self.assertEqual(resp.status, 200)
        finally:
            conn.close()
        seen = UpstreamHandler.seen[-1]
        self.assertEqual(seen["host_count"], 1, "上游只能看到一个 Host")
        self.assertEqual(
            header(seen["headers"], "Host"),
            "127.0.0.1:{}".format(self.upstream.server_address[1]),
        )
        self.assertNotIn("evil.example", json.dumps(seen["headers"]))
        for name in seen["header_names"]:
            self.assertEqual(name, name.lower(), "转发头键必须统一小写：{}".format(name))


    # ── 401 / 502 ──
    def test_missing_token_is_401_and_upstream_is_not_called(self):
        status, headers, body = self.call()
        self.assertEqual(status, 401, body)
        self.assertEqual(header(headers, "WWW-Authenticate"), "Bearer")
        self.assertEqual(len(UpstreamHandler.seen), 0, "无 token 的请求不能到达上游")

    def test_invalid_token_is_401(self):
        status, _, body = self.call(headers=self.auth("bad-token"))
        self.assertEqual(status, 401, body)
        self.assertEqual(len(UpstreamHandler.seen), 0)

    def test_userinfo_missing_sub_is_401(self):
        status, _, body = self.call(headers=self.auth("nosub-token"))
        self.assertEqual(status, 401, body)
        self.assertEqual(len(UpstreamHandler.seen), 0)

    def test_userinfo_5xx_is_502_fail_closed(self):
        status, _, body = self.call(headers=self.auth("boom-token"))
        self.assertEqual(status, 502, body)
        self.assertEqual(len(UpstreamHandler.seen), 0)

    def test_aud_mismatch_is_401(self):
        status, _, body = self.call(headers=self.auth("aud-token"))
        self.assertEqual(status, 401, body)
        self.assertEqual(len(UpstreamHandler.seen), 0)

    def test_aud_missing_is_401_in_default_strict_mode(self):
        status, _, body = self.call(headers=self.auth("noaud-token"))
        self.assertEqual(status, 401, body)
        self.assertEqual(len(UpstreamHandler.seen), 0)

    def test_aud_missing_is_allowed_in_if_present_mode(self):
        with self.custom_proxy(aud_mode="if-present") as proxy:
            status, _, body = self.call(
                headers=self.auth("noaud-token"), proxy=proxy
            )
            self.assertEqual(status, 200, body)
            # if-present 不是 off：有 aud 但不匹配仍然拒绝。
            status, _, body = self.call(
                headers=self.auth("aud-token"), proxy=proxy
            )
            self.assertEqual(status, 401, body)

    def test_aud_off_skips_aud_check(self):
        with self.custom_proxy(aud_mode="off") as proxy:
            for token in ("noaud-token", "aud-token"):
                status, _, body = self.call(headers=self.auth(token), proxy=proxy)
                self.assertEqual(status, 200, "{}: {}".format(token, body))

    # ── 身份主键：默认 sub；email 必须 email_verified=true ──
    def test_default_identity_is_sub_and_unverified_email_is_dropped(self):
        status, _, body = self.call(headers=self.auth("unverified-email-token"))
        self.assertEqual(status, 200, body)
        seen = UpstreamHandler.seen[-1]
        self.assertEqual(header(seen["headers"], "X-UI-User"), "sub-7")
        self.assertIsNone(
            header(seen["headers"], "X-UI-Email"),
            "未验证的 email 不能作为展示身份",
        )

    def test_email_identity_requires_verified_email(self):
        with self.custom_proxy(identity="email") as proxy:
            # email_verified=false
            status, _, body = self.call(
                headers=self.auth("unverified-email-token"), proxy=proxy
            )
            self.assertEqual(status, 401, body)
            # 只有 sub、没有 email
            status, _, body = self.call(
                headers=self.auth("nested-token"), proxy=proxy
            )
            self.assertEqual(status, 401, body)
            # 字符串 "true" 不等于布尔 True
            status, _, body = self.call(
                headers=self.auth("string-verified-token"), proxy=proxy
            )
            self.assertEqual(status, 401, body)
            self.assertEqual(len(UpstreamHandler.seen), 0)

    def test_email_identity_uses_verified_email(self):
        with self.custom_proxy(identity="email") as proxy:
            status, _, body = self.call(headers=self.auth(), proxy=proxy)
            self.assertEqual(status, 200, body)
        seen = UpstreamHandler.seen[-1]
        self.assertEqual(header(seen["headers"], "X-UI-User"), "alice@example.com")
        self.assertEqual(header(seen["headers"], "X-UI-Email"), "alice@example.com")

    # ── 上游状态透传 / 流式 / SSE 超时 ──
    def test_upstream_error_is_passed_through(self):
        status, _, body = self.call(path="/boom", headers=self.auth())
        self.assertEqual(status, 500)
        self.assertIn(b"upstream boom", body)

    def test_streaming_response_is_relayed(self):
        status, headers, body = self.call(path="/stream", headers=self.auth())
        self.assertEqual(status, 200)
        self.assertEqual(header(headers, "Content-Type"), "text/event-stream")
        self.assertEqual(body, b"data: one\n\ndata: two\n\n")

    def test_sse_endpoint_uses_sse_timeout_not_request_timeout(self):
        """普通读超时 0.3s，SSE 读超时 3s：分块间隔 0.6s 仍必须全部收到。"""
        with self.custom_proxy(timeout=0.3, sse_timeout=3.0) as proxy:
            status, headers, body = self.call(
                path="/api/events",
                headers=self.auth(),
                proxy=proxy,
                timeout=10,
            )
        self.assertEqual(status, 200)
        self.assertEqual(header(headers, "Content-Type"), "text/event-stream")
        self.assertEqual(
            body, b"data: one\n\ndata: two\n\ndata: three\n\n"
        )

    def test_api_stream_alias_is_treated_as_sse(self):
        self.assertTrue(oidc_proxy.is_sse_path("/api/events"))
        self.assertTrue(oidc_proxy.is_sse_path("/api/stream"))
        self.assertTrue(oidc_proxy.is_sse_path("/api/events/"))
        self.assertFalse(oidc_proxy.is_sse_path("/api/streams"))
        self.assertFalse(oidc_proxy.is_sse_path("/api/admin/rbac"))

    def test_ordinary_request_still_uses_short_timeout(self):
        """普通路径不能借用 SSE 的长超时。"""
        with self.custom_proxy(timeout=0.3, sse_timeout=3.0) as proxy:
            status, _, body = self.call(
                path="/slow", headers=self.auth(), proxy=proxy
            )
        self.assertEqual(status, 502, body)

    # ── roles 归一 / 嵌套 claim / preferred_username ──
    def test_string_roles_are_normalized(self):
        status, _, _ = self.call(headers=self.auth("string-roles"))
        self.assertEqual(status, 200)
        self.assertEqual(
            header(UpstreamHandler.seen[-1]["headers"], "X-UI-Roles"),
            "operator,auditor,owner",
        )

    def test_nested_roles_claim_uses_sub_identity(self):
        with self.custom_proxy(
            client_id="", roles_claim="realm_access.roles"
        ) as proxy:
            status, _, _ = self.call(
                headers=self.auth("nested-token"), proxy=proxy
            )
            self.assertEqual(status, 200)
        seen = UpstreamHandler.seen[-1]
        # 默认 OIDC_IDENTITY=sub：preferred_username 不再当身份主键。
        self.assertEqual(header(seen["headers"], "X-UI-User"), "sub-2")
        self.assertEqual(header(seen["headers"], "X-UI-Roles"), "admin")

    def test_groups_claim_and_email_identity(self):
        """OIDC 常见声明名 `groups`：`OIDC_ROLES_CLAIM` 可配置，别硬编码 roles。"""
        with self.custom_proxy(
            client_id="", roles_claim="groups", identity="email"
        ) as proxy:
            status, _, _ = self.call(
                headers=self.auth("groups-token"), proxy=proxy
            )
            self.assertEqual(status, 200)
        seen = UpstreamHandler.seen[-1]
        # 显式 OIDC_IDENTITY=email 且 email_verified=true：保持 users.json 的 email 映射。
        self.assertEqual(header(seen["headers"], "X-UI-User"), "erin@example.com")
        self.assertEqual(header(seen["headers"], "X-UI-Email"), "erin@example.com")
        # 小写、去重、丢弃不认识的角色。
        self.assertEqual(header(seen["headers"], "X-UI-Roles"), "owner,viewer")

    # ── 配置边界：只监听回环、密钥必填 ──
    def test_listen_must_be_loopback(self):
        for value in ("0.0.0.0:7789", "192.168.1.5:7789", "[::]:7789"):
            with self.assertRaises(oidc_proxy.ConfigError):
                oidc_proxy.parse_listen(value)
        self.assertEqual(oidc_proxy.parse_listen("127.0.0.1:7789"), ("127.0.0.1", 7789))
        self.assertEqual(oidc_proxy.parse_listen("localhost:0"), ("localhost", 0))

    def test_config_requires_secret_and_loopback_upstream(self):
        base = {
            "OIDC_USERINFO_URL": "https://idp.example.com/userinfo",
            "UI_UPSTREAM_URL": "http://127.0.0.1:7788",
            "OIDC_LISTEN": "127.0.0.1:7789",
        }
        with self.assertRaises(oidc_proxy.ConfigError):
            oidc_proxy.load_config(dict(base))
        # 非回环 + 明文 http：拒绝启动（Bearer/代理密钥会明文过网）。
        for value in (
            "http://10.0.0.9:7788",
            "http://192.168.1.5:7788",
            "http://ui.internal:7788",
        ):
            with self.assertRaises(oidc_proxy.ConfigError):
                oidc_proxy.load_config(
                    dict(base, UI_PROXY_SECRET="s", UI_UPSTREAM_URL=value)
                )
        with self.assertRaises(oidc_proxy.ConfigError):
            oidc_proxy.load_config(
                dict(base, UI_PROXY_SECRET="s", OIDC_LISTEN="0.0.0.0:7789")
            )
        for key, value in (
            ("OIDC_AUD_MODE", "bogus"),
            ("OIDC_IDENTITY", "username"),
            ("OIDC_TIMEOUT", "0"),
            ("OIDC_SSE_TIMEOUT", "abc"),
        ):
            with self.assertRaises(oidc_proxy.ConfigError):
                oidc_proxy.load_config(dict(base, UI_PROXY_SECRET="s", **{key: value}))

        cfg = oidc_proxy.load_config(dict(base, UI_PROXY_SECRET="s"))
        self.assertEqual(cfg.roles_claim, "roles")
        self.assertEqual(cfg.aud_mode, "strict")
        self.assertEqual(cfg.identity, "sub")
        self.assertEqual(cfg.timeout, 5.0)
        self.assertEqual(cfg.sse_timeout, 300.0)
        self.assertEqual(cfg.upstream_authority, "127.0.0.1:7788")

        # OIDC_TIMEOUT 是新名字；OIDC_USERINFO_TIMEOUT 作为旧名仍可用。
        cfg_new = oidc_proxy.load_config(
            dict(base, UI_PROXY_SECRET="s", OIDC_TIMEOUT="8", OIDC_SSE_TIMEOUT="600")
        )
        self.assertEqual(cfg_new.timeout, 8.0)
        self.assertEqual(cfg_new.sse_timeout, 600.0)
        cfg_legacy = oidc_proxy.load_config(
            dict(base, UI_PROXY_SECRET="s", OIDC_USERINFO_TIMEOUT="7")
        )
        self.assertEqual(cfg_legacy.timeout, 7.0)

        # 非回环 upstream 只能走 https；默认端口不重复写进 Host。
        remote = oidc_proxy.load_config(
            dict(
                base,
                UI_PROXY_SECRET="s",
                UI_UPSTREAM_URL="https://ui.example.com:8443",
            )
        )
        self.assertEqual(remote.upstream_scheme, "https")
        self.assertEqual(remote.upstream_authority, "ui.example.com:8443")
        remote_default = oidc_proxy.Config(
            userinfo_url=base["OIDC_USERINFO_URL"],
            client_id="",
            roles_claim="roles",
            upstream_url="https://ui.example.com",
            proxy_secret="s",
            listen="127.0.0.1:0",
        )
        self.assertEqual(remote_default.upstream_port, 443)
        self.assertEqual(remote_default.upstream_authority, "ui.example.com")

        # 回环 http 仍允许。
        self.assertEqual(
            oidc_proxy.parse_upstream("http://localhost:7788").hostname, "localhost"
        )
        self.assertEqual(
            oidc_proxy.parse_upstream("http://[::1]:7788").hostname, "::1"
        )

    def test_process_refuses_non_loopback_http_upstream(self):
        """非回环 + http 必须在绑定端口前退出（exit 2），而不是带病启动。"""
        env = dict(os.environ)
        env.update(
            {
                "OIDC_USERINFO_URL": "https://idp.example.com/userinfo",
                "UI_UPSTREAM_URL": "http://10.0.0.9:7788",
                "UI_PROXY_SECRET": SECRET,
                "OIDC_LISTEN": "127.0.0.1:{}".format(free_port()),
            }
        )
        proc = subprocess.run(
            [sys.executable, str(HERE / "oidc_proxy.py")],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        )
        self.assertEqual(proc.returncode, 2)
        err = proc.stderr.decode("utf-8", "replace")
        self.assertIn("UI_UPSTREAM_URL", err)
        self.assertIn("https", err)
        self.assertNotIn(SECRET, err, "启动错误信息里不能出现代理密钥")

    # ── 不打印 token：起真进程，抓 stdout/stderr ──
    def test_process_does_not_print_token(self):
        port = free_port()
        env = dict(os.environ)
        env.update(
            {
                "OIDC_USERINFO_URL": self.config.userinfo_url,
                "OIDC_CLIENT_ID": "ui-enterprise",
                "OIDC_ROLES_CLAIM": "roles",
                "UI_UPSTREAM_URL": self.config.upstream_url,
                "UI_PROXY_SECRET": SECRET,
                "OIDC_LISTEN": "127.0.0.1:{}".format(port),
            }
        )
        proc = subprocess.Popen(
            [sys.executable, str(HERE / "oidc_proxy.py")],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            deadline = time.time() + 5
            while time.time() < deadline:
                try:
                    conn = HTTPConnection("127.0.0.1", port, timeout=1)
                    conn.request("GET", "/echo", headers={"Authorization": "Bearer good-token"})
                    resp = conn.getresponse()
                    resp.read()
                    conn.close()
                    if resp.status == 200:
                        break
                except OSError:
                    time.sleep(0.05)
            else:
                self.fail("代理没有在 5 秒内起来")
        finally:
            proc.terminate()
            try:
                out, err = proc.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                out, err = proc.communicate()
        combined = (out + err).decode("utf-8", "replace")
        self.assertNotIn("good-token", combined, "日志/启动信息里不能出现 token")
        self.assertIn("listening", combined)
        self.assertNotIn(SECRET, combined, "日志里不能出现代理密钥")


if __name__ == "__main__":
    # Windows 管道默认 GBK：中文用例名在 CI 日志里会乱码。
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass
    unittest.main(verbosity=2)
