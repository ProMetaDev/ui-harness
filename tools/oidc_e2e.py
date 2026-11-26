#!/usr/bin/env python3
"""OIDC userinfo 适配层 → `ui serve --trusted-proxy` → `/api/admin` 的真实 e2e。

这条链路在单元测试里是分开测的（`adapters/oidc/test_proxy.py` 用 mock 上游），
本脚本把三段接起来跑：

    [本进程内 mock IdP /userinfo]
        ↑ Authorization: Bearer <token>
    [adapters/oidc/oidc_proxy.py]（子进程，127.0.0.1）
        ↓ X-UI-User / X-UI-Roles / X-UI-Proxy-Secret
    [out/ui.exe serve --trusted-proxy loopback --token off]（子进程，127.0.0.1）
        ↓ GET/PUT /api/admin/*
    [本脚本的 HTTP 断言]

覆盖：

- 无 Bearer / 无效 token → 401；
- viewer 读 `/api/admin/rbac` → 403，owner → 200；
- viewer token + 伪造 `X-UI-User`/`X-UI-Roles`/`X-UI-Proxy-Secret` 仍按 viewer → 403；
- owner `PUT /api/admin/roles`、`PUT /api/admin/users` → 200，并落 `rbac/*` journal；
- mock IdP / 代理 / `ui serve` 的日志不含 token 或代理密钥；
- 代理以 `OIDC_IDENTITY=email` 跑（mock `email_verified=true`），验证既有
  `.ui/users.json` 的 email 键映射；生产默认仍是 `sub`；
- 可选：Chrome CDP 注入 `Authorization`，打开 `/admin` 断言 `userForm`/`matrixBody`。

边界：

- 不需要网络：mock IdP 在 127.0.0.1，`ui serve` 用 `--mock` 剧本，不调用真实模型；
- 没有 `out/ui.exe`（或 `--ui` 指的文件）时明确 SKIP；
- 没有 Chrome/Edge 时只 SKIP 浏览器部分，HTTP 断言仍然跑；
- 代理/服务都用子进程启动，结束（含失败路径）时统一杀掉。
"""

import argparse
import base64
import json
import os
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request
from http.client import HTTPConnection, HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEVNULL = subprocess.DEVNULL

# Windows 的 Python 默认用 GBK 写管道；中文断言在 CI/终端里会变成乱码。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError):
        pass

# 测试专用假 token：不是真实凭据。代理密钥每轮随机生成——这样「日志里没有密钥」
# 的断言不会因为源码里的固定字符串碰巧命中而变成假阳性/假阴性。
OWNER_TOKEN = "owner-token"
VIEWER_TOKEN = "viewer-token"
INVALID_TOKEN = "not-a-real-token"
OIDC_CLIENT_ID = "ui-oidc-e2e"
PROXY_SECRET = "oidc-e2e-proxy-secret-" + secrets.token_hex(8)

IDP_USERS = {
    OWNER_TOKEN: {
        "sub": "owner-sub",
        "email": "owner@example.com",
        "email_verified": True,
        "aud": OIDC_CLIENT_ID,
        "groups": ["owner"],
    },
    VIEWER_TOKEN: {
        "sub": "viewer-sub",
        "email": "viewer@example.com",
        "email_verified": True,
        "aud": OIDC_CLIENT_ID,
        "groups": ["viewer"],
    },
}

CHROME_CANDIDATES = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
]


# ───────────────────────────── 通用小工具 ─────────────────────────────


def free_port():
    sock = socket.socket()
    try:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]
    finally:
        sock.close()


def http(port, method, path, token=None, headers=None, body=None, timeout=20):
    """发一个到 127.0.0.1:<port> 的请求，返回 (status, headers, bytes)。"""
    conn = HTTPConnection("127.0.0.1", port, timeout=timeout)
    out_headers = dict(headers or {})
    if token is not None:
        out_headers["Authorization"] = "Bearer " + token
    payload = None
    if body is not None:
        payload = json.dumps(body, ensure_ascii=False).encode("utf-8")
        out_headers["Content-Type"] = "application/json; charset=utf-8"
        out_headers["Content-Length"] = str(len(payload))
    try:
        conn.request(method, path, body=payload, headers=out_headers)
        resp = conn.getresponse()
        data = resp.read()
        return resp.status, {k.lower(): v for k, v in resp.getheaders()}, data
    finally:
        conn.close()


def parse_json(data):
    try:
        return json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return None


def read_text(path):
    try:
        return Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def tail(path, limit=1200):
    text = read_text(path)
    return text[-limit:]


class Checks:
    def __init__(self):
        self.passed = 0
        self.failed = []
        self.warned = 0

    def ok(self, name, condition, detail=""):
        if condition:
            self.passed += 1
            print("  ok   {}".format(name))
        else:
            self.failed.append(name)
            print("  FAIL {}".format(name))
            if detail:
                print("       {}".format(detail))
        return bool(condition)

    def warn(self, name, detail=""):
        """浏览器等可选部分是 WARN：记录下来，但不改变 HTTP e2e 的退出码。"""
        self.warned += 1
        print("  WARN {}".format(name))
        if detail:
            print("       {}".format(detail))


# ───────────────────────────── mock IdP ─────────────────────────────


class IdpHandler(BaseHTTPRequestHandler):
    """只实现 `/userinfo`；日志只记方法/路径/状态码，绝不记 Authorization。"""

    protocol_version = "HTTP/1.1"
    server_version = "mock-oidc-idp"
    sys_version = ""

    def log_message(self, fmt, *args):
        pass

    def _json(self, status, payload):
        data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)
            self.wfile.flush()
        self.close_connection = True

    def do_GET(self):
        path = urllib.parse.urlsplit(self.path).path
        if path != "/userinfo":
            self.server.note("GET {} 404".format(path))
            return self._json(404, {"error": "not_found"})
        raw = self.headers.get("Authorization", "")
        token = raw[7:].strip() if raw.lower().startswith("bearer ") else ""
        info = IDP_USERS.get(token)
        if info is None:
            self.server.note("GET /userinfo 401")
            return self._json(401, {"error": "invalid_token"})
        self.server.note("GET /userinfo 200")
        return self._json(200, info)

    do_HEAD = do_GET


class MockIdP(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address):
        super().__init__(address, IdpHandler)
        self.lines = []
        self.lock = threading.Lock()

    def note(self, line):
        with self.lock:
            self.lines.append(line)


# ───────────────────────────── 进程管理 ─────────────────────────────


class Proc:
    def __init__(self, name, cmd, logdir, env=None):
        self.name = name
        self.path = os.path.join(logdir, name + ".log")
        self.file = open(self.path, "wb")
        self.proc = subprocess.Popen(
            cmd,
            stdout=self.file,
            stderr=subprocess.STDOUT,
            cwd=str(ROOT),
            env=env,
        )

    def alive(self):
        return self.proc.poll() is None

    def stop(self):
        if self.proc.poll() is None:
            if os.name == "nt":
                subprocess.run(
                    ["taskkill", "/T", "/F", "/PID", str(self.proc.pid)],
                    stdout=DEVNULL,
                    stderr=DEVNULL,
                )
            else:
                self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        try:
            self.file.close()
        except OSError:
            pass


def find_ui(explicit):
    candidates = []
    if explicit:
        candidates.append(Path(explicit))
    if os.environ.get("UI_EXE"):
        candidates.append(Path(os.environ["UI_EXE"]))
    candidates.extend(
        [
            ROOT / "out" / "ui.exe",
            ROOT / "out" / "ui",
            ROOT / "target" / "release" / "ui.exe",
            ROOT / "target" / "release" / "ui",
        ]
    )
    if os.name != "nt":
        candidates.append(
            Path.home() / "ui-target" / "x86_64-pc-windows-gnu" / "release" / "ui.exe"
        )
    for path in candidates:
        if path.is_file():
            return str(path.resolve())
    return None


def find_browser():
    explicit = os.environ.get("CHROME_PATH") or os.environ.get("CHROME")
    if explicit and os.path.isfile(explicit):
        return explicit
    for path in CHROME_CANDIDATES:
        if os.path.isfile(path):
            return path
    return None


# ───────────────────────────── Chrome CDP ─────────────────────────────


class _WebSocket:
    """最小 WebSocket 客户端：只够 CDP 用（文本帧、客户端掩码）。"""

    def __init__(self, url):
        if not url.startswith("ws://"):
            raise RuntimeError("只支持 ws:// target: " + url)
        host_path = url[5:]
        host, path = host_path.split("/", 1)
        host, port = host.split(":")
        self.sock = socket.create_connection((host, int(port)), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            "GET /{} HTTP/1.1\r\nHost: {}:{}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            "Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).format(path, host, port, key)
        self.sock.sendall(request.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.sock.recv(4096)
        if b"101" not in buf.split(b"\r\n", 1)[0]:
            raise RuntimeError(
                "websocket handshake failed: " + buf[:200].decode(errors="replace")
            )

    def send(self, text):
        data = text.encode()
        header = bytearray([0x81])
        n = len(data)
        if n < 126:
            header.append(0x80 | n)
        elif n < 65536:
            header.append(0x80 | 126)
            header += n.to_bytes(2, "big")
        else:
            header.append(0x80 | 127)
            header += n.to_bytes(8, "big")
        mask = os.urandom(4)
        header += mask
        self.sock.sendall(header + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def recv(self):
        def readn(n):
            out = b""
            while len(out) < n:
                chunk = self.sock.recv(n - len(out))
                if not chunk:
                    raise RuntimeError("websocket closed")
                out += chunk
            return out

        while True:
            b0, b1 = readn(2)
            opcode = b0 & 0x0F
            masked = b1 & 0x80
            n = b1 & 0x7F
            if n == 126:
                n = int.from_bytes(readn(2), "big")
            elif n == 127:
                n = int.from_bytes(readn(8), "big")
            mask = readn(4) if masked else None
            payload = readn(n)
            if mask:
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            if opcode == 0x8:
                raise RuntimeError("websocket closed by peer")
            if opcode == 0x9:
                continue
            if opcode in (0x1, 0x2):
                return payload.decode()

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class CDP:
    def __init__(self, ws):
        self.ws = ws
        self.next_id = 0

    def call(self, method, params=None, timeout=20):
        self.next_id += 1
        current = self.next_id
        self.ws.send(
            json.dumps({"id": current, "method": method, "params": params or {}})
        )
        end = time.time() + timeout
        while time.time() < end:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == current:
                if "error" in msg:
                    raise RuntimeError("{}: {}".format(method, msg["error"]))
                return msg.get("result", {})
        raise RuntimeError("CDP timeout: " + method)

    def evaluate(self, expression):
        result = self.call(
            "Runtime.evaluate",
            {
                "expression": expression,
                "returnByValue": True,
                "awaitPromise": True,
            },
        )
        res = result.get("result", {})
        if res.get("subtype") == "error":
            raise RuntimeError(res.get("description", "JS error"))
        return res.get("value")


def wait_url(url, timeout=20):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            with urllib.request.urlopen(url, timeout=1) as resp:
                return resp.read()
        except Exception as exc:  # noqa: BLE001 - 只是等端口起来
            last = exc
            time.sleep(0.2)
    raise RuntimeError("timeout waiting {}: {}".format(url, last))


def kill_tree(proc):
    if proc is None or proc.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/T", "/F", "/PID", str(proc.pid)],
            stdout=DEVNULL,
            stderr=DEVNULL,
        )
    else:
        proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


def run_browser(proxy_port, checks):
    """可选：Chrome CDP 给 `ui serve` 的页面请求注入 Authorization。

    返回 "PASS" / "SKIP" / "WARN"，浏览器失败不影响 HTTP e2e 的退出码。
    """
    browser = find_browser()
    if not browser:
        print("SKIP browser: 未找到 Chrome/Edge（HTTP e2e 已覆盖，浏览器部分跳过）")
        return "SKIP"

    debug_port = free_port()
    profile = tempfile.mkdtemp(prefix="ui-oidc-chrome-")
    chrome = None
    ws = None
    try:
        chrome = subprocess.Popen(
            [
                browser,
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--remote-debugging-port={}".format(debug_port),
                "--user-data-dir={}".format(profile),
                "--no-first-run",
                "--no-default-browser-check",
                "about:blank",
            ],
            stdout=DEVNULL,
            stderr=DEVNULL,
        )
        targets = json.loads(wait_url("http://127.0.0.1:{}/json/list".format(debug_port)))
        pages = [t for t in targets if t.get("type") == "page"]
        page = next((t for t in pages if str(proxy_port) in t.get("url", "")), None)
        if not page and pages:
            page = pages[0]
        if not page:
            raise RuntimeError("没有 page target: " + repr(targets))

        ws = _WebSocket(page["webSocketDebuggerUrl"])
        cdp = CDP(ws)
        cdp.call("Network.enable", {})
        # CDP 的 extra headers 会附加到这个 target 之后的所有请求（导航 + 页面 fetch）。
        # 若某个 Chrome 版本不把 Authorization 带在主文档上，页面会落到 401 空态，
        # 浏览器断言会 WARN，但 HTTP e2e 仍然给出确定结论。
        cdp.call(
            "Network.setExtraHTTPHeaders",
            {"headers": {"Authorization": "Bearer " + OWNER_TOKEN}},
        )
        cdp.call("Page.enable", {})
        cdp.call(
            "Page.navigate",
            {"url": "http://127.0.0.1:{}/admin".format(proxy_port)},
        )

        deadline = time.time() + 12
        dom = None
        while time.time() < deadline:
            try:
                dom = cdp.evaluate(
                    """(() => {
                      const m = document.getElementById('matrixBody');
                      const b = document.body;
                      return {
                        title: document.title,
                        ready: document.readyState,
                        userForm: !!document.getElementById('userForm'),
                        matrixBody: !!document.getElementById('matrixBody'),
                        matrix: m ? m.innerText.slice(0, 400) : '',
                        body: b ? b.innerText.slice(0, 600) : ''
                      };
                    })()"""
                )
            except Exception:  # noqa: BLE001 - 导航中 evaluate 会短暂失败
                dom = None
            if dom and dom.get("userForm") and dom.get("matrixBody"):
                if "加载中" not in dom.get("matrix", ""):
                    break
            time.sleep(0.3)

        if not dom:
            raise RuntimeError("页面没有可读取的 DOM")
        mounted = bool(dom.get("userForm") and dom.get("matrixBody"))
        if not mounted:
            checks.warn(
                "浏览器 /admin 未挂载（userForm/matrixBody 缺失；不影响 HTTP 结论）",
                repr({k: dom.get(k) for k in ("title", "userForm", "matrixBody")}),
            )
            return "WARN"
        checks.ok("浏览器 /admin 挂载：userForm + matrixBody 存在", True)
        # 如果 CDP 的 Authorization 真的生效，矩阵里应该有 owner 行、身份区有 owner。
        authenticated = ("owner" in dom.get("matrix", "")) or (
            "owner@example.com" in dom.get("body", "")
        )
        if authenticated:
            print("  ok   浏览器 owner 身份已注入（矩阵/身份区可见 owner）")
        else:
            print(
                "  WARN 浏览器页面已挂载，但未看到 owner 数据（CDP 请求头注入可能未生效）"
            )
            print("       matrix={!r}".format(dom.get("matrix", "")[:160]))
        return "PASS"
    except Exception as exc:  # noqa: BLE001 - 浏览器是可选断言
        print("WARN browser 未完成: {}".format(exc))
        return "WARN"
    finally:
        if ws is not None:
            ws.close()
        kill_tree(chrome)
        shutil.rmtree(profile, ignore_errors=True)


# ───────────────────────────── 主流程 ─────────────────────────────


def spawn_proxy(upstream_port, proxy_port, idp_port, logdir):
    env = dict(os.environ)
    env.update(
        {
            "OIDC_USERINFO_URL": "http://127.0.0.1:{}/userinfo".format(idp_port),
            "OIDC_CLIENT_ID": OIDC_CLIENT_ID,
            # 这条 e2e 沿用 users.json 的 email 映射，所以要显式声明 email 身份；
            # 适配层要求 email_verified=true（mock IdP 已返回 true）。生产建议用 sub。
            "OIDC_IDENTITY": "email",
            # mock IdP 用的是 groups 声明：顺便验证 claim 名可配。
            "OIDC_ROLES_CLAIM": "groups",
            "UI_UPSTREAM_URL": "http://127.0.0.1:{}".format(upstream_port),
            "UI_PROXY_SECRET": PROXY_SECRET,
            "OIDC_LISTEN": "127.0.0.1:{}".format(proxy_port),
            "OIDC_TIMEOUT": "3",
            # 不让子进程的 Python 输出受终端编码影响。
            "PYTHONIOENCODING": "utf-8",
            "PYTHONUTF8": "1",
        }
    )
    return Proc(
        "oidc-proxy",
        [sys.executable, str(ROOT / "adapters" / "oidc" / "oidc_proxy.py")],
        logdir,
        env,
    )


def spawn_ui(ui_path, upstream_port, workspace, scenario, logdir):
    return Proc(
        "ui-serve",
        [
            ui_path,
            "serve",
            "--port",
            str(upstream_port),
            "--workspace",
            str(workspace),
            "--mock",
            str(scenario),
            "--token",
            "off",
            "--trusted-proxy",
            "loopback",
            "--trusted-proxy-secret",
            PROXY_SECRET,
            "--yes",
            "--no-splash",
        ],
        logdir,
    )


def wait_ready(proxy_port, procs, timeout=30):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        for item in procs:
            if not item.alive():
                raise RuntimeError(
                    "{} 提前退出（exit {}）：\n{}".format(
                        item.name, item.proc.returncode, tail(item.path)
                    )
                )
        try:
            status, _, data = http(
                proxy_port, "GET", "/api/ready", token=OWNER_TOKEN, timeout=2
            )
            if status == 200:
                return
            last = (status, data[:200])
        except (OSError, HTTPException) as exc:
            last = repr(exc)
        time.sleep(0.15)
    raise RuntimeError("代理链路 30 秒内没就绪：{}".format(last))


def main():
    parser = argparse.ArgumentParser(description="OIDC userinfo → ui serve → /api/admin e2e")
    parser.add_argument("--ui", help="ui.exe 路径（默认找 out/ui.exe）")
    parser.add_argument("--no-browser", action="store_true", help="跳过 Chrome CDP 部分")
    parser.add_argument("--keep", action="store_true", help="保留临时工作区便于排查")
    args = parser.parse_args()

    ui_path = find_ui(args.ui)
    if not ui_path:
        print("SKIP: 找不到 ui.exe。先运行 bash tools/build_windows.sh 并把产物放到 out/ui.exe，")
        print("      或用 --ui /path/to/ui.exe（也可设 UI_EXE）。")
        return 0

    print("== OIDC e2e ==")
    print("ui      {}".format(ui_path))
    # 工作目录放在仓库的 out/ 下：`ui serve` 要写 `.ui/journal.jsonl`，而某些
    # 受限环境会拒绝子进程写系统临时目录（Windows 上实测 os error 5）。out/ 是
    # gitignored 的构建/测试目录，也保证 e2e 的落盘都在仓库内。
    scratch_root = ROOT / "out"
    scratch_root.mkdir(parents=True, exist_ok=True)
    workdir = tempfile.mkdtemp(prefix="oidc-e2e-", dir=str(scratch_root))
    workspace = os.path.join(workdir, "workspace")
    os.makedirs(workspace, exist_ok=True)
    scenario = os.path.join(workdir, "scenario.json")
    with open(scenario, "w", encoding="utf-8") as handle:
        json.dump({"model": "scripted", "steps": [{"content": "hello"}]}, handle)

    idp = MockIdP(("127.0.0.1", 0))
    idp_thread = threading.Thread(target=idp.serve_forever, daemon=True)
    idp_thread.start()
    idp_port = idp.server_address[1]

    ui_port = free_port()
    proxy_port = free_port()
    procs = []
    browser_status = "SKIP"
    checks = Checks()

    try:
        procs.append(spawn_ui(ui_path, ui_port, workspace, scenario, workdir))
        procs.append(spawn_proxy(ui_port, proxy_port, idp_port, workdir))
        wait_ready(proxy_port, procs)
        checks.ok("代理 → ui serve 链路就绪（/api/ready 200）", True)

        # ── 读：401 / 403 / 200 ──
        status, headers, _ = http(proxy_port, "GET", "/api/admin/rbac")
        checks.ok(
            "无 Bearer → 401",
            status == 401 and headers.get("www-authenticate") == "Bearer",
            "status={} headers={}".format(status, headers),
        )

        status, _, _ = http(proxy_port, "GET", "/api/admin/rbac", token=INVALID_TOKEN)
        checks.ok("无效 token → 401", status == 401, "status={}".format(status))

        status, _, data = http(proxy_port, "GET", "/api/admin/rbac", token=VIEWER_TOKEN)
        checks.ok(
            "viewer 读 /api/admin/rbac → 403",
            status == 403,
            "status={} body={!r}".format(status, data[:200]),
        )

        status, _, data = http(proxy_port, "GET", "/api/admin/rbac", token=OWNER_TOKEN)
        owner_rbac = parse_json(data) or {}
        checks.ok(
            "owner 读 /api/admin/rbac → 200 且矩阵含 owner",
            status == 200 and "owner" in (owner_rbac.get("roles") or {}),
            "status={} body={!r}".format(status, data[:300]),
        )

        # ── 剥离：viewer token + 伪造身份头（连代理密钥都伪造）──
        forged = {
            "X-UI-User": "attacker@example.com",
            "X-UI-Roles": "owner",
            "X-UI-Proxy-Secret": PROXY_SECRET,
        }
        status, _, data = http(
            proxy_port,
            "GET",
            "/api/admin/rbac",
            token=VIEWER_TOKEN,
            headers=forged,
        )
        checks.ok(
            "viewer token + 伪造 X-UI-Roles/X-UI-User/Proxy-Secret 仍按 viewer → 403",
            status == 403,
            "status={} body={!r}".format(status, data[:200]),
        )

        # 反向确认：伪造身份头不能绕开 Bearer。
        status, _, _ = http(
            proxy_port, "GET", "/api/admin/rbac", headers=forged
        )
        checks.ok("只有伪造身份头、无 Bearer → 401", status == 401, "status={}".format(status))

        # ── 写：owner PUT → 200 + journal 审计 ──
        status, _, data = http(
            proxy_port,
            "PUT",
            "/api/admin/roles",
            token=OWNER_TOKEN,
            body={"name": "operator", "write": True, "audit": False, "manage": False},
        )
        role_reply = parse_json(data) or {}
        checks.ok(
            "owner PUT /api/admin/roles → 200",
            status == 200 and role_reply.get("ok") is True,
            "status={} body={!r}".format(status, data[:300]),
        )

        second_user = "e2e-operator@example.com"
        status, _, data = http(
            proxy_port,
            "PUT",
            "/api/admin/users",
            token=OWNER_TOKEN,
            body={"id": second_user, "role": "operator", "display": "E2E Operator"},
        )
        user_reply = parse_json(data) or {}
        checks.ok(
            "owner PUT /api/admin/users → 200",
            status == 200 and user_reply.get("ok") is True,
            "status={} body={!r}".format(status, data[:300]),
        )

        status, _, data = http(proxy_port, "GET", "/api/admin/users", token=OWNER_TOKEN)
        users = parse_json(data) or {}
        ids = [u.get("id") for u in (users.get("users") or [])]
        checks.ok(
            "GET /api/admin/users 能看到刚写入的用户",
            status == 200 and second_user in ids,
            "status={} ids={}".format(status, ids),
        )

        status, _, _ = http(
            proxy_port,
            "PUT",
            "/api/admin/users",
            token=VIEWER_TOKEN,
            body={"id": "viewer-write@example.com", "role": "operator"},
        )
        checks.ok("viewer PUT /api/admin/users → 403", status == 403, "status={}".format(status))

        # ── journal：rbac/* 审计 + 不含凭据 ──
        journal_path = os.path.join(workspace, ".ui", "journal.jsonl")
        journal = read_text(journal_path)
        checks.ok(
            "journal 出现 rbac/role-update 审计",
            "rbac/role-update" in journal,
            "journal={!r}".format(journal[-400:]),
        )
        checks.ok(
            "journal 出现 rbac/user-update 审计",
            "rbac/user-update" in journal,
            "journal={!r}".format(journal[-400:]),
        )
        checks.ok(
            "journal 的 actor 是 owner@example.com",
            "owner@example.com" in journal,
            "journal={!r}".format(journal[-400:]),
        )
        checks.ok(
            "journal 不含 token / 代理密钥",
            not any(
                value in journal
                for value in (OWNER_TOKEN, VIEWER_TOKEN, INVALID_TOKEN, PROXY_SECRET)
            ),
        )

        # ── 可选浏览器部分 ──
        if args.no_browser:
            print("SKIP browser: --no-browser")
            browser_status = "SKIP"
        else:
            browser_status = run_browser(proxy_port, checks)

        # 浏览器跑完再收尾；日志检查必须在进程退出/flush 之后。
    finally:
        for item in reversed(procs):
            item.stop()
        idp.shutdown()
        idp.server_close()
        idp_thread.join(timeout=2)

    # ── 日志：proxy / ui serve / mock IdP 都不能出现 token 或密钥 ──
    print("== 日志脱敏 ==")
    log_files = [(item.name, item.path) for item in procs]
    idp_log = os.path.join(workdir, "mock-idp.log")
    with open(idp_log, "w", encoding="utf-8") as handle:
        handle.write("\n".join(idp.lines) + "\n")
    log_files.append(("mock-idp", idp_log))

    secrets = [
        ("owner token", OWNER_TOKEN),
        ("viewer token", VIEWER_TOKEN),
        ("无效 token 样本", INVALID_TOKEN),
        ("代理密钥", PROXY_SECRET),
    ]
    for name, path in log_files:
        text = read_text(path)
        leaked = [label for label, value in secrets if value in text]
        checks.ok(
            "{} 日志不含 token/代理密钥".format(name),
            not leaked,
            "泄漏项：{}".format("、".join(leaked)) if leaked else "",
        )

    proxy_log = read_text(os.path.join(workdir, "oidc-proxy.log"))
    checks.ok(
        "代理日志有请求记录（方法 + 路径 + 状态码）",
        "GET /api/admin/rbac" in proxy_log and " 403" in proxy_log,
        "proxy log={!r}".format(proxy_log[-400:]),
    )

    if args.keep:
        print("临时目录保留：{}".format(workdir))
    else:
        shutil.rmtree(workdir, ignore_errors=True)

    print()
    if checks.failed:
        print(
            "FAIL OIDC e2e: {} 项通过，{} 项失败，{} 项警告（浏览器：{}）".format(
                checks.passed, len(checks.failed), checks.warned, browser_status
            )
        )
        for name in checks.failed:
            print("  - {}".format(name))
        return 1
    print(
        "PASS OIDC e2e: {} 项通过，{} 项警告（浏览器：{}）".format(
            checks.passed, checks.warned, browser_status
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
