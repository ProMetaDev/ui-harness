#!/usr/bin/env python3
"""M3 最小容器 runner（L2）：Docker + 标准库 HTTP。

边界：
- 只挂载 workspace：/workspace；
- rootfs read-only，/tmp tmpfs；
- 默认 --network none；
- 内存/CPU/PID/输出/超时上限；
- 结束时 --rm；超时 docker kill。
"""
import argparse
import hmac
import json
import os
import subprocess
import sys
import tempfile
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MAX_BODY = 1024 * 1024
MAX_OUTPUT = 8 * 1024 * 1024


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
        out_f = tempfile.TemporaryFile()
        err_f = tempfile.TemporaryFile()
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
            "-w", "/workspace",
            self.image,
            "sh", "-c", command,
        ]
        timed_out = False
        try:
            p = subprocess.Popen(cmd, stdout=out_f, stderr=err_f)
            try:
                code = p.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                subprocess.run(["docker", "kill", name],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                try:
                    code = p.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    p.kill()
                    code = -1
        finally:
            out_f.seek(0)
            err_f.seek(0)
            out = out_f.read(MAX_OUTPUT + 1)
            err = err_f.read(MAX_OUTPUT + 1)
            out_f.close()
            err_f.close()
        truncated = len(out) > MAX_OUTPUT or len(err) > MAX_OUTPUT
        return {
            "ok": (not timed_out and code == 0),
            "status": code,
            "stdout": out[:MAX_OUTPUT].decode(errors="replace"),
            "stderr": err[:MAX_OUTPUT].decode(errors="replace"),
            "truncated": truncated,
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

        def send_json(self, code, obj):
            body = json.dumps(obj, ensure_ascii=False).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/healthz":
                return self.send_json(200, {"ok": True})
            self.send_json(404, {"ok": False, "error": "not found"})

        def do_POST(self):
            if self.path != "/execute":
                return self.send_json(404, {"ok": False, "error": "not found"})
            if not self.auth():
                return self.send_json(401, {"ok": False, "error": "unauthorized"})
            n = int(self.headers.get("Content-Length", "0"))
            if n > MAX_BODY:
                return self.send_json(413, {"ok": False, "error": "body too large"})
            try:
                req = json.loads(self.rfile.read(n) or b"{}")
                out = runner.execute(req)
                self.send_json(200, out)
            except Exception as exc:
                self.send_json(400, {"ok": False, "error": str(exc)})

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
