#!/usr/bin/env python3
"""`adapters/oidc/oidc_proxy.py` 的回归：本地 mock userinfo + mock ui serve。

覆盖：200 / 401 / 502 / 上游状态透传 / 剥离客户端 X-UI-* / 不转发 token /
roles 归一 / 嵌套 claim / 只监听回环 / 不打印 token。
"""

import json
import os
import socket
import subprocess
import sys
import threading
import time
import unittest
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
        "roles": ["operator", "auditor"],
    },
    "nested-token": {
        "sub": "sub-2",
        "preferred_username": "bob",
        "realm_access": {"roles": ["admin"]},
    },
    "nosub-token": {"email": "nobody@example.com"},
    "string-roles": {
        "sub": "sub-3",
        "email": "carol@example.com",
        "roles": "operator, Auditor, bad role, owner",
    },
    "aud-token": {
        "sub": "sub-4",
        "email": "dave@example.com",
        "aud": "some-other-client",
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
                "body": body,
            }
        )
        path = self.path.split("?")[0]
        if path == "/echo":
            return self._send(200, {"ok": True, "headers": dict(self.headers.items())})
        if path == "/boom":
            return self._send(500, {"ok": False, "error": "upstream boom"})
        if path == "/stream":
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
        return self._send(404, {"ok": False, "error": "no route"})

    do_GET = _handle
    do_POST = _handle
    do_PUT = _handle
    do_DELETE = _handle


def serve(handler):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
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


class ProxyTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.userinfo = serve(UserinfoHandler)
        cls.upstream = serve(UpstreamHandler)
        cls.config = oidc_proxy.Config(
            userinfo_url="http://127.0.0.1:{}/userinfo".format(cls.userinfo.server_address[1]),
            client_id="ui-enterprise",
            roles_claim="roles",
            upstream_url="http://127.0.0.1:{}".format(cls.upstream.server_address[1]),
            proxy_secret=SECRET,
            listen="127.0.0.1:0",
            timeout=3.0,
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

    def call(self, method="GET", path="/echo", headers=None, body=None):
        conn = HTTPConnection("127.0.0.1", self.proxy.server_address[1], timeout=10)
        try:
            conn.request(method, path, body=body, headers=headers or {})
            resp = conn.getresponse()
            data = resp.read()
            return resp.status, dict(resp.getheaders()), data
        finally:
            conn.close()

    def auth(self, token="good-token"):
        return {"Authorization": "Bearer " + token}

    # ── 200：注入身份头、剥离客户端头、不转发 token ──
    def test_valid_token_injects_identity_and_strips_client_headers(self):
        status, _, body = self.call(
            headers=dict(
                self.auth(),
                **{
                    "X-UI-User": "evil@example.com",
                    "X-UI-Roles": "owner",
                    "X-UI-Proxy-Secret": "forged-secret",
                    "X-UI-Request-Id": "attacker",
                }
            ),
        )
        self.assertEqual(status, 200, body)
        seen = UpstreamHandler.seen[-1]
        self.assertEqual(header(seen["headers"], "X-UI-User"), "alice@example.com")
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
        self.assertEqual(header(seen["headers"], "X-UI-User"), "alice@example.com")
        self.assertEqual(header(seen["headers"], "X-UI-Roles"), "operator,auditor")

    def test_post_body_and_client_host_are_forwarded(self):
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
        # Host 用客户端视角的 Host：ui serve 的 CSRF 检查按 Origin == http://Host 比对。
        self.assertEqual(
            header(seen["headers"], "Host"),
            "127.0.0.1:{}".format(self.proxy.server_address[1]),
        )
        self.assertEqual(seen["path"], "/echo")

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

    # ── 上游状态透传 / 流式 ──
    def test_upstream_error_is_passed_through(self):
        status, _, body = self.call(path="/boom", headers=self.auth())
        self.assertEqual(status, 500)
        self.assertIn(b"upstream boom", body)

    def test_streaming_response_is_relayed(self):
        status, headers, body = self.call(path="/stream", headers=self.auth())
        self.assertEqual(status, 200)
        self.assertEqual(header(headers, "Content-Type"), "text/event-stream")
        self.assertEqual(body, b"data: one\n\ndata: two\n\n")

    # ── roles 归一 / 嵌套 claim / preferred_username ──
    def test_string_roles_are_normalized(self):
        status, _, _ = self.call(headers=self.auth("string-roles"))
        self.assertEqual(status, 200)
        self.assertEqual(
            header(UpstreamHandler.seen[-1]["headers"], "X-UI-Roles"),
            "operator,auditor,owner",
        )

    def test_nested_roles_claim_and_preferred_username(self):
        config = oidc_proxy.Config(
            userinfo_url=self.config.userinfo_url,
            client_id="",
            roles_claim="realm_access.roles",
            upstream_url=self.config.upstream_url,
            proxy_secret=SECRET,
            listen="127.0.0.1:0",
            timeout=3.0,
        )
        proxy = oidc_proxy.ProxyServer(("127.0.0.1", 0), config)
        proxy.daemon_threads = True
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        try:
            conn = HTTPConnection("127.0.0.1", proxy.server_address[1], timeout=10)
            try:
                conn.request("GET", "/echo", headers={"Authorization": "Bearer nested-token"})
                resp = conn.getresponse()
                resp.read()
                self.assertEqual(resp.status, 200)
            finally:
                conn.close()
        finally:
            proxy.shutdown()
            proxy.server_close()
        seen = UpstreamHandler.seen[-1]
        self.assertEqual(header(seen["headers"], "X-UI-User"), "bob")
        self.assertEqual(header(seen["headers"], "X-UI-Roles"), "admin")

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
        with self.assertRaises(oidc_proxy.ConfigError):
            oidc_proxy.load_config(
                dict(base, UI_PROXY_SECRET="s", UI_UPSTREAM_URL="http://10.0.0.9:7788")
            )
        with self.assertRaises(oidc_proxy.ConfigError):
            oidc_proxy.load_config(
                dict(base, UI_PROXY_SECRET="s", OIDC_LISTEN="0.0.0.0:7789")
            )
        cfg = oidc_proxy.load_config(dict(base, UI_PROXY_SECRET="s"))
        self.assertEqual(cfg.roles_claim, "roles")

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
    unittest.main(verbosity=2)
