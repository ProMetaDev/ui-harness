#!/usr/bin/env bash
# M3 runner live 测试：真实 Docker + 本地 HTTP runner。
#
# 除原有的 workspace/network/timeout/token/escape 外，还钉住二轮评审的
# N-P0-2 四项：
#   1. workspace 里的 secrets/、.ui/、.git/、.env 在容器内不可见；
#   2. 负数/缺失/非数字 Content-Length 与 chunked 请求被拒绝；
#   3. 无限输出在 MAX_OUTPUT 处被实时截断（不是先落盘再截）；
#   4. 超时/输出超限后不留容器。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

IMAGE="${UI_RUNNER_IMAGE:-python:3.11-slim}"
if ! command -v docker >/dev/null 2>&1 || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "SKIP: docker/$IMAGE 不可用"
  exit 0
fi
PORT="${UI_RUNNER_PORT:-18089}"
WS=$(mktemp -d /tmp/ui-runner-ws.XXXXXX)
LOG=/tmp/ui-runner.log
TOKEN=test-token

# 遮蔽测试的哨兵：容器里只要读到这些字符串，就说明宿主内容漏进去了。
mkdir -p "$WS/secrets" "$WS/.ui" "$WS/.git" "$WS/repo"
printf 'SECRET-SENTINEL-secrets\n' >"$WS/secrets/secret.txt"
printf 'SECRET-SENTINEL-ui\n' >"$WS/.ui/private.txt"
printf 'SECRET-SENTINEL-git\n' >"$WS/.git/config"
printf 'ENV-SENTINEL\n' >"$WS/.env"
printf 'WORKSPACE-KEEP\n' >"$WS/keep.txt"
# .git 在 git worktree 里是普通文件；单独验证文件型遮蔽不能靠空卷。
printf 'GITFILE-SENTINEL\n' >"$WS/repo/.git"
printf 'REPO-KEEP\n' >"$WS/repo/keep.txt"

python3 adapters/runner/runner.py --port "$PORT" --workspace-root "$WS" --image "$IMAGE" --token "$TOKEN" >"$LOG" 2>&1 &
SRV=$!
cleanup() {
  kill "$SRV" 2>/dev/null || true
  docker ps -q --filter "name=ui-runner-" | xargs -r docker kill >/dev/null 2>&1 || true
  rm -rf "$WS"
}
trap cleanup EXIT

for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then break; fi
  sleep 0.2
done
curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null

# fail closed：没有 token 必须拒绝启动（空 token = 对本机所有进程开放 /execute）。
# 用 timeout 兜住"万一它没拒绝、反而起来了"的情况（124 = 真起来了并挂住）。
if command -v timeout >/dev/null 2>&1; then
  set +e
  timeout 5 python3 adapters/runner/runner.py \
    --port "$((PORT + 1))" --workspace-root "$WS" --image "$IMAGE" >/dev/null 2>&1
  NO_TOKEN_CODE=$?
  set -e
  if [ "$NO_TOKEN_CODE" -eq 0 ] || [ "$NO_TOKEN_CODE" -eq 124 ]; then
    echo "FAIL: 缺 token 时 runner 没有拒绝启动（退出码 $NO_TOKEN_CODE）"
    exit 1
  fi
fi

# 原有契约：workspace 可写、默认无网络、超时识别、token 校验、路径越界拒绝。
python3 adapters/runner/test_live.py \
  --base "http://127.0.0.1:$PORT" --token "$TOKEN" --workspace "$WS"

# N-P0-2 新增：遮蔽、请求体校验、输出流控。
python3 - "$PORT" "$TOKEN" "$WS" <<'PY'
import json
import socket
import sys
import time
import urllib.error
import urllib.request

port = int(sys.argv[1])
token = sys.argv[2]
ws = sys.argv[3]
base = f"http://127.0.0.1:{port}"
MAX_OUTPUT = 8 * 1024 * 1024


def post(body, timeout=60):
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        base + "/execute", data=data,
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + token},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, json.loads(r.read())


def raw_post(extra_headers, body=b"", timeout=5):
    payload = (
        "POST /execute HTTP/1.1\r\n"
        "Host: 127.0.0.1\r\n"
        f"Authorization: Bearer {token}\r\n"
        "Content-Type: application/json\r\n"
        + extra_headers
        + "Connection: close\r\n\r\n"
    ).encode() + body
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    try:
        s.sendall(payload)
        data = b""
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            data += chunk
        return data
    except socket.timeout:
        raise AssertionError("非法请求没有及时返回：runner 可能卡在 read(-1) 或等待 chunked 正文")
    finally:
        s.close()


def status_of(resp):
    line = resp.split(b"\r\n", 1)[0]
    parts = line.split()
    assert len(parts) >= 2, resp[:200]
    return int(parts[1])


# ---- 1. 挂载遮蔽：目录三项 + 根 .env ----
mask_cmd = r'''
for d in secrets .ui .git; do
  printf 'DIR %s count=%s\n' "$d" "$(ls -A "/workspace/$d" | wc -l)"
done
printf 'ENV='; cat /workspace/.env 2>&1 || true; echo
printf 'TRY_SECRET='; cat /workspace/secrets/secret.txt 2>&1 || true; echo
printf 'TRY_UI='; cat /workspace/.ui/private.txt 2>&1 || true; echo
printf 'TRY_GIT='; cat /workspace/.git/config 2>&1 || true; echo
printf 'KEEP='; cat /workspace/keep.txt
'''
code, out = post({"workspace": ws, "command": mask_cmd})
assert code == 200 and out["ok"], out
text = out["stdout"] + out["stderr"]
assert text.count("count=0") == 3, text
for name in ("secrets", ".ui", ".git"):
    assert f"DIR {name} count=0" in text, text
for sentinel in ("SECRET-SENTINEL-secrets", "SECRET-SENTINEL-ui", "SECRET-SENTINEL-git", "ENV-SENTINEL"):
    assert sentinel not in text, text
assert "KEEP=WORKSPACE-KEEP" in text, text

# .git 为普通文件（git worktree）时也必须被 /dev/null 遮蔽。
code, out = post({"workspace": ws + "/repo", "command": r'''
if [ -d /workspace/.git ]; then echo GIT_TYPE=dir; else echo GIT_TYPE=notdir; fi
printf 'GIT_CONTENT='; cat /workspace/.git 2>&1 || true; echo
printf 'KEEP='; cat /workspace/keep.txt
'''})
assert code == 200 and out["ok"], out
text = out["stdout"] + out["stderr"]
assert "GITFILE-SENTINEL" not in text, text
assert "GIT_TYPE=notdir" in text, text
assert "KEEP=REPO-KEEP" in text, text

# ---- 2. Content-Length / Transfer-Encoding 校验 ----
assert status_of(raw_post("Content-Length: -1\r\n")) == 400, "负数 Content-Length 必须 400"
assert status_of(raw_post("Content-Length: abc\r\n")) == 400, "非数字 Content-Length 必须 400"
assert status_of(raw_post("")) == 400, "缺失 Content-Length 必须 400"
assert status_of(raw_post("Content-Length: 1048577\r\n")) == 413, "超过 MAX_BODY 必须 413"
assert status_of(raw_post("Content-Length: 99999999999999999999\r\n")) == 413
assert status_of(raw_post("Transfer-Encoding: chunked\r\n", b"2\r\n{}\r\n0\r\n\r\n")) in (400, 411), \
    "chunked 必须 400/411"

# ---- 3. 执行期输出流控：无限输出必须在 MAX_OUTPUT 处停下 ----
started = time.monotonic()
code, out = post({"workspace": ws, "timeout": 20, "command": "yes OVERSIZE-OUTPUT"}, timeout=45)
elapsed = time.monotonic() - started
assert code == 200, (code, out.get("error"))
assert out["truncated"] is True, out
assert out["timedOut"] is False, "输出超限应立即 kill，而不是等到 timeout"
stdout_bytes = out["stdout"].encode()
assert len(stdout_bytes) <= MAX_OUTPUT, len(stdout_bytes)
assert len(stdout_bytes) >= MAX_OUTPUT - 64 * 1024, len(stdout_bytes)
assert elapsed < 15, f"截断耗时 {elapsed:.1f}s，疑似没有在输出期间截断"

print("PASS runner live: workspace/network/timeout/token/escape/mask/body-validation/output-cap")
PY

# 输出超限/超时都必须清掉容器；留下一个就是越权执行的持久面。
if docker ps -q --filter "name=ui-runner-" | grep -q .; then
  echo "FAIL: 测试结束后仍有 ui-runner 容器残留"
  docker ps --filter "name=ui-runner-"
  exit 1
fi
