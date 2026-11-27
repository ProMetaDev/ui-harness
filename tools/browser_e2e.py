#!/usr/bin/env python3
"""真实浏览器 e2e：Chrome DevTools Protocol，仅标准库。

- 没有 Chrome 时 SKIP；
- 启动 ui.exe + headless Chrome；
- 用 CDP Runtime.evaluate 检查页面真的挂载了外壳/变更按钮；
- 结束用 taskkill /T 清理进程树。
"""
import base64
import hashlib
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CHROME_CANDIDATES = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
]


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_url(url, timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with urllib.request.urlopen(url, timeout=1) as r:
                return r.read()
        except Exception:
            time.sleep(0.2)
    raise RuntimeError("timeout waiting " + url)


class WS:
    def __init__(self, url):
        assert url.startswith("ws://")
        host_path = url[5:]
        host, path = host_path.split("/", 1)
        host, port = host.split(":")
        self.sock = socket.create_connection((host, int(port)), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET /{path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.sock.recv(4096)
        if b"101" not in buf.split(b"\r\n", 1)[0]:
            raise RuntimeError("websocket handshake failed: " + buf[:200].decode(errors="replace"))

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


def cdp(ws, method, params):
    ws.send(json.dumps({"id": 1, "method": method, "params": params}))
    while True:
        msg = json.loads(ws.recv())
        if msg.get("id") == 1:
            if "error" in msg:
                raise RuntimeError(msg["error"])
            return msg.get("result", {})


def evaluate(ws, expr):
    result = cdp(ws, "Runtime.evaluate", {
        "expression": expr, "returnByValue": True, "awaitPromise": True,
    })
    res = result.get("result", {})
    if res.get("subtype") == "error":
        raise RuntimeError(res.get("description", "JS error"))
    return res.get("value")


def kill_tree(pid):
    subprocess.run(["taskkill", "/T", "/F", "/PID", str(pid)],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def main():
    browser = next((p for p in CHROME_CANDIDATES if os.path.exists(p)), None)
    if not browser:
        print("SKIP: no Chrome/Edge")
        return 0
    ui = os.path.join(ROOT, "out", "ui.exe")
    if len(sys.argv) >= 3 and sys.argv[1] == "--ui":
        ui = os.path.abspath(sys.argv[2])
    if not os.path.exists(ui):
        print("SKIP: ui.exe missing (build windows first)")
        return 0

    wsdir = os.path.join(ROOT, "out", "browser-e2e")
    os.makedirs(wsdir, exist_ok=True)
    scenario = os.path.join(wsdir, "scenario.json")
    with open(scenario, "w", encoding="utf-8") as f:
        json.dump({"model": "scripted", "steps": [{"content": "hello"}]}, f)

    port = free_port()
    debug = free_port()
    profile = tempfile.mkdtemp(prefix="ui-chrome-")
    server = subprocess.Popen(
        [ui, "serve", "--port", str(port), "--workspace", wsdir,
         "--mock", scenario, "--token", "off", "--yes", "--no-splash"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    chrome = None
    try:
        wait_url(f"http://127.0.0.1:{port}/api/state")
        chrome = subprocess.Popen(
            [browser, "--headless=new", "--disable-gpu", "--no-sandbox",
             "--window-size=1440,900",
             f"--remote-debugging-port={debug}", f"--user-data-dir={profile}",
             "--no-first-run", "--no-default-browser-check",
             f"http://127.0.0.1:{port}/?shot=1"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        targets = json.loads(wait_url(f"http://127.0.0.1:{debug}/json/list"))
        pages = [t for t in targets if t.get("type") == "page"]
        # Chrome 可能先创建一个 about:blank 页面；必须选真正导航到 ui 的 target。
        page = next((t for t in pages if str(port) in t.get("url", "")), None)
        if not page and pages:
            page = pages[0]
        if not page:
            raise RuntimeError("no page target: " + repr(targets))
        ws = WS(page["webSocketDebuggerUrl"])
        # 不依赖 Chrome 的初始 target 是否已经加载；显式导航一次。
        cdp(ws, "Page.navigate", {"url": f"http://127.0.0.1:{port}/"})
        cdp(ws, "Page.bringToFront", {})
        time.sleep(0.8)
        value = evaluate(ws, """(() => {
          const b = document.getElementById('diffBtn');
          return {
            shell: !!document.querySelector('.shell'),
            diff: !!b,
            text: b ? b.textContent : '',
            title: document.title
          };
        })()""")
        if not (value and value.get("shell") and value.get("diff") and "变更" in value.get("text", "")):
            raise RuntimeError("page not mounted: " + repr(value))
        # 点一下“变更”，确认面板真的能打开（无 checkpoint 时应显示明确空态）。
        opened = evaluate(ws, """(async () => {
          const b = document.getElementById('diffBtn');
          b.focus();
          b.click();
          await new Promise(r => setTimeout(r, 500));
          const o = document.getElementById('diffOverlay');
          return {hidden: o.hidden, text: o.textContent.slice(0, 120), active: document.activeElement && document.activeElement.id};
        })()""")
        if not opened or opened.get("hidden") is not False:
            raise RuntimeError("diff overlay did not open: " + repr(opened))
        if opened.get("active") != "diffClose":
            raise RuntimeError("diff overlay did not take focus: " + repr(opened))
        trapped = evaluate(ws, """(() => {
          const o = document.getElementById('diffOverlay');
          const items = Array.from(o.querySelectorAll('button:not(:disabled),select:not(:disabled),input:not(:disabled),[tabindex]:not([tabindex=\"-1\"])'))
            .filter((el) => el.getClientRects().length > 0);
          if (!items.length) return false;
          items[items.length - 1].focus();
          document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Tab', bubbles: true}));
          return document.activeElement === items[0];
        })()""")
        if trapped is not True:
            raise RuntimeError("diff overlay did not trap Tab focus")
        closed = evaluate(ws, """(() => {
          document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}));
          const o = document.getElementById('diffOverlay');
          return {hidden: o.hidden, active: document.activeElement && document.activeElement.id};
        })()""")
        if not closed or closed.get("hidden") is not True or closed.get("active") != "diffBtn":
            raise RuntimeError("diff overlay did not restore focus: " + repr(closed))
        # 右栏预算条必须跟随真实 token 数；旧实现读已删除的 #sBud，永远停在 0。
        meter = evaluate(ws, """(async () => {
          document.getElementById('tok').textContent = '50000';
          await new Promise(r => setTimeout(r, 1200));
          return {pct: document.getElementById('rpct').textContent, width: document.getElementById('rbar').style.width};
        })()""")
        if not meter or meter.get("pct") in (None, "0%") or meter.get("width") in (None, "", "0%"):
            raise RuntimeError("budget meter did not update: " + repr(meter))
        # UU 必须服从真实状态：审批卡在时保持 waiting，撤掉后回到 idle。
        mood = evaluate(ws, """(() => {
          const approvals = document.getElementById('approvals');
          const card = document.createElement('div');
          card.className = 'approval';
          approvals.appendChild(card);
          const live = document.getElementById('liveStrip');
          if (live) live.dataset.busy = '0';
          window.__uiMood('thinking');
          window.__uiMoodSync();
          const waiting = document.querySelector('.uu').dataset.mood;
          card.remove();
          window.__uiMoodSync();
          return {waiting, idle: document.querySelector('.uu').dataset.mood};
        })()""")
        if not mood or mood.get("waiting") != "waiting" or mood.get("idle") != "idle":
            raise RuntimeError("UU mood did not follow approval state: " + repr(mood))
        # 管理面：导航后检查真实 DOM，而不是只 curl HTML。
        cdp(ws, "Page.navigate", {"url": f"http://127.0.0.1:{port}/admin"})
        time.sleep(0.8)
        admin = evaluate(ws, """(() => ({
          title: document.title,
          text: document.body ? document.body.innerText : '',
          cards: document.querySelectorAll('.card').length,
          userForm: !!document.getElementById('userForm'),
          matrix: !!document.getElementById('matrixBody'),
          userBody: !!document.getElementById('userBody')
        }))()""")
        if (
            not admin
            or "管理面" not in admin.get("text", "")
            or admin.get("cards", 0) < 4
            or not admin.get("userForm")
            or not admin.get("matrix")
            or not admin.get("userBody")
        ):
            raise RuntimeError("admin page not mounted: " + repr(admin))
        print(
            f"PASS browser e2e: title={value.get('title')} diff=ok focus=ok trap=ok budget=ok mood=ok admin=ok forms=ok"
        )
        return 0
    finally:
        if chrome:
            kill_tree(chrome.pid)
        kill_tree(server.pid)
        # 临时 profile 由系统清理；不在这里递归删，避免竞态。


if __name__ == "__main__":
    sys.exit(main())
