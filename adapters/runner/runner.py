#!/usr/bin/env python3
"""M3 最小容器 runner（L2）：Docker + 标准库 HTTP。

边界：
- 只挂载 workspace：/workspace；
- workspace 内的 secrets/、.ui/、.git/ 用匿名空卷遮蔽，根 .env 用 /dev/null；
- rootfs read-only，/tmp tmpfs；
- 默认 --network none；
- 内存/CPU/PID/输出/超时上限；
- 结束时 --rm；超时或输出超限 docker kill。
"""
import argparse
import hmac
import json
import os
import subprocess
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MAX_BODY = 1024 * 1024
MAX_OUTPUT = 8 * 1024 * 1024
READ_CHUNK = 64 * 1024
MASKED_PATHS = ("secrets", ".ui", ".git")


def mask_mounts(ws):
    """返回 workspace bind mount 之后要追加的遮蔽挂载。

    容器里的 `sh -c` 不受 ui-core / Rust 工具层（protected.rs）的路径策略
    约束，所以 secrets/、.ui/、.git/ 必须在挂载层遮蔽，不能指望容器内进程
    自己遵守宿主工具层的规则。

    - 目录：匿名空卷 + volume-nocopy。volume-nocopy 是必要的：没有它时
      Docker 可能把目标目录里的宿主内容复制进新卷，遮蔽反而变成搬运。
    - 文件：/dev/null 只读 bind，读取得到空内容。
    - 路径不存在：不挂载。给不存在的路径挂空卷会让 Docker 在宿主 workspace
      里创建 root 所有的空目录，污染用户工作区；此时也没有内容可泄漏。
    """
    args = []
    for name in MASKED_PATHS:
        p = ws / name
        if p.is_dir():
            args += ["--mount", f"type=volume,destination=/workspace/{name},volume-nocopy"]
        elif p.exists():
            args += ["-v", f"/dev/null:/workspace/{name}:ro"]
    env = ws / ".env"
    if env.is_dir():
        args += ["--mount", "type=volume,destination=/workspace/.env,volume-nocopy"]
    elif env.exists():
        args += ["-v", "/dev/null:/workspace/.env:ro"]
    return args


class Runner:
    def __init__(self, workspace_root, image, token):
        self.root = Path(workspace_root).resolve()
        self.image = image
        self.token = token

    def check_workspace(self, raw):
        p = Path(raw).resolve()
        if p != self.root and self.root not in p.parents:
            raise ValueError("workspace 越出 runner root")
        if not p.is_dir():
            raise ValueError("workspace 不存在")
        return p

    def execute(self, req):
        ws = self.check_workspace(req.get("workspace", str(self.root)))
        command = req.get("command", "")
        if not isinstance(command, str) or not command.strip():
            raise ValueError("缺少 command")
        timeout = min(max(int(req.get("timeout", 30)), 1), 300)
        name = "ui-runner-" + uuid.uuid4().hex[:12]
        cmd = [
            "docker", "run", "--rm", "--name", name,
            # rootless Docker 下容器 root 不一定能写 workspace；用宿主 uid 运行业务进程。
            "--user", f"{os.getuid()}:{os.getgid()}" if hasattr(os, "getuid") else "65534:65534",
            "--network", "none",
            "--memory", "256m",
            "--cpus", "1",
            "--pids-limit", "128",
            "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges",
            "--read-only",
            "--tmpfs", "/tmp:rw,noexec,nosuid,size=64m",
            "-v", f"{ws}:/workspace:rw",
        ]
        # 关键顺序：先 bind workspace，再在它上面盖空卷，宿主内容才不可见。
        cmd += mask_mounts(ws)
        cmd += [
            "-w", "/workspace",
            self.image,
            "sh", "-c", command,
        ]

        out = bytearray()
        err = bytearray()
        overflow = threading.Event()
        kill_lock = threading.Lock()

        def kill_container():
            # 杀 docker CLI 不会停容器；输出超限和超时都必须 docker kill。
            with kill_lock:
                try:
                    subprocess.run(
                        ["docker", "kill", name],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        timeout=5,
                    )
                except (OSError, subprocess.SubprocessError):
                    pass

        def drain(stream, sink):
            # 边读边计数：到 MAX_OUTPUT 立刻停止追加并 kill。
            # 不写 TemporaryFile，所以不存在"先无限落盘、结束后再截断"的窗口。
            read = getattr(stream, "read1", stream.read)
            try:
                while True:
                    chunk = read(READ_CHUNK)
                    if not chunk:
                        return
                    if overflow.is_set():
                        continue
                    room = MAX_OUTPUT - len(sink)
                    if room > 0:
                        sink.extend(chunk[:room])
                    if len(chunk) > room:
                        overflow.set()
                        kill_container()
            except (OSError, ValueError):
                pass
            finally:
                try:
                    stream.close()
                except OSError:
                    pass

        timed_out = False
        p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        readers = [
            threading.Thread(target=drain, args=(p.stdout, out), daemon=True),
            threading.Thread(target=drain, args=(p.stderr, err), daemon=True),
        ]
        for t in readers:
            t.start()
        try:
            code = p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            kill_container()
            try:
                code = p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()
                code = -1
        for t in readers:
            t.join(timeout=5)
        if p.poll() is None:
            # docker kill 失败时别把容器/CLI 留在后台。
            p.kill()
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        return {
            "ok": (not timed_out and code == 0),
            "status": code,
            "stdout": bytes(out).decode(errors="replace"),
            "stderr": bytes(err).decode(errors="replace"),
            "truncated": overflow.is_set(),
            "timedOut": timed_out,
            "sandbox": "container:" + name,
        }


def handler(runner):
    class H(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def auth(self):
            # 没有 token 时**不**放行：能否启动由 main() 先挡一次，这里再挡一次——
            # 少任何一道，"忘传 token"就等于把任意命令执行开给本机所有进程。
            if not runner.token:
                return False
            got = self.headers.get("Authorization", "")
            # 定长比较：token 是唯一凭据，别按字节提前返回。
            return hmac.compare_digest(got, "Bearer " + runner.token)

        def send_json(self, code, obj, close=False):
            body = json.dumps(obj, ensure_ascii=False).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            if close:
                # 校验失败时请求体可能还没读；不关连接会把剩余字节当成下一个请求。
                self.send_header("Connection", "close")
                self.close_connection = True
            self.end_headers()
            self.wfile.write(body)

        def body_length(self):
            """返回 (n, error_code)。只接受唯一的 ASCII 十进制非负 Content-Length。"""
            if self.headers.get("Transfer-Encoding") is not None:
                # chunked / 任何 Transfer-Encoding 都不支持；411 = Length Required。
                return None, 411
            lengths = self.headers.get_all("Content-Length") or []
            if len(lengths) != 1:
                return None, 400
            raw = lengths[0].strip()
            # isdigit() 拒掉负号、正号、小数点、0x、空串；isascii() 挡掉 Unicode 数字。
            if not raw or not raw.isascii() or not raw.isdigit():
                return None, 400
            # 先按位数挡超长数字，避免 int() 处理巨大字符串；10 位十进制已远超 MAX_BODY。
            if len(raw) > 10:
                return None, 413
            n = int(raw)
            if n > MAX_BODY:
                return None, 413
            return n, None

        def do_GET(self):
            if self.path == "/healthz":
                return self.send_json(200, {"ok": True})
            self.send_json(404, {"ok": False, "error": "not found"})

        def do_POST(self):
            if self.path != "/execute":
                return self.send_json(404, {"ok": False, "error": "not found"}, close=True)
            if not self.auth():
                return self.send_json(401, {"ok": False, "error": "unauthorized"}, close=True)
            n, error = self.body_length()
            if error is not None:
                message = {
                    400: "invalid Content-Length",
                    411: "Content-Length required",
                    413: "body too large",
                }.get(error, "bad request")
                return self.send_json(error, {"ok": False, "error": message}, close=True)
            body = self.rfile.read(n)
            if len(body) != n:
                return self.send_json(
                    400, {"ok": False, "error": "incomplete body"}, close=True
                )
            try:
                req = json.loads(body or b"{}")
                if not isinstance(req, dict):
                    raise ValueError("请求体必须是 JSON 对象")
            except ValueError as exc:
                return self.send_json(400, {"ok": False, "error": str(exc)}, close=True)
            try:
                out = runner.execute(req)
            except ValueError as exc:
                return self.send_json(400, {"ok": False, "error": str(exc)})
            except Exception as exc:
                return self.send_json(500, {"ok": False, "error": str(exc)})
            self.send_json(200, out)

    return H


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=18080)
    p.add_argument("--workspace-root", required=True)
    p.add_argument("--image", default="python:3.11-slim")
    p.add_argument(
        "--token",
        default=os.environ.get("RUNNER_TOKEN", ""),
        help="Bearer token；必须非空（也可用 RUNNER_TOKEN 环境变量）",
    )
    a = p.parse_args()
    # fail closed：token 是唯一身份边界，空 token 等于对本机所有进程开放 /execute。
    # 不做"空 token 就跳过校验"的降级——那种默认值正是安全审计里最常见的一条。
    token = (a.token or "").strip()
    if not token:
        p.error("必须提供非空的 --token 或 RUNNER_TOKEN（空 token 时拒绝启动）")
    runner = Runner(a.workspace_root, a.image, token)
    srv = ThreadingHTTPServer(("127.0.0.1", a.port), handler(runner))
    print(f"runner on 127.0.0.1:{a.port} image={a.image}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
