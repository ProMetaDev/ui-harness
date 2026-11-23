import argparse
import json
import urllib.error
import urllib.request
from pathlib import Path


def post(base, token, body):
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        base + "/execute", data=data,
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + token},
    )
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read())


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--base", required=True)
    p.add_argument("--token", required=True)
    p.add_argument("--workspace", required=True)
    a = p.parse_args()
    ws = Path(a.workspace)

    code, out = post(a.base, a.token, {"workspace": str(ws), "command": "echo hi > out.txt && echo done"})
    assert code == 200 and out["ok"], out
    assert (ws / "out.txt").read_text().strip() == "hi"
    assert "done" in out["stdout"]

    # 缺 token 必须 401。
    req = urllib.request.Request(a.base + "/execute", data=b"{}",
                                 headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req, timeout=5)
        raise AssertionError("missing token accepted")
    except urllib.error.HTTPError as e:
        assert e.code == 401, e.code

    # 路径越界必须 400。
    code, out = post(a.base, a.token, {"workspace": "/etc", "command": "true"})
    assert code == 400 and not out["ok"], out

    # 默认无网络：连接外部端口应失败。
    code, out = post(a.base, a.token, {
        "workspace": str(ws), "timeout": 5,
        "command": "python -c \"import socket; socket.create_connection(('1.1.1.1',80),1)\"",
    })
    assert code == 200 and not out["ok"], out

    # 超时必须被识别，并清理容器。
    code, out = post(a.base, a.token, {"workspace": str(ws), "timeout": 1, "command": "sleep 5"})
    assert code == 200 and out["timedOut"], out
    print("PASS runner live: workspace/network/timeout/token/escape")


if __name__ == "__main__":
    main()
