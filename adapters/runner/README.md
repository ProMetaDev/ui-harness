# 容器 runner（M3，L2）

> 不属于 `ui-core`；需要 Docker。core 仍 std-only。

## 启动

```bash
python3 adapters/runner/runner.py \
  --port 18089 \
  --workspace-root /srv/ui/workspaces \
  --image python:3.11-slim \
  --token "$RUNNER_TOKEN"
```

## API

```text
GET  /healthz
POST /execute
Authorization: Bearer <token>
{
  "workspace": "/srv/ui/workspaces/s1",
  "command": "echo hi > out.txt",
  "timeout": 30
}
```

## 隔离

- 只挂载 workspace 到 `/workspace:rw`；
- rootfs `--read-only`；
- `/tmp` 为 `noexec,nosuid` tmpfs；
- `--network none`；
- `--memory 256m --cpus 1 --pids-limit 128`；
- `--cap-drop ALL --security-opt no-new-privileges`；
- 宿主 uid/gid 运行业务进程（rootless Docker 下 workspace 可写）；
- 超时 `docker kill`；
- stdout/stderr 8 MiB 上限。

## 测试

```bash
bash tools/verify_runner_live.sh
```

实测：

```text
PASS runner live: workspace/network/timeout/token/escape
```

## 未实现

- mTLS；
- seccomp/AppArmor 自定义 profile；
- 流式输出；
- 每任务独立网络 allowlist；
- GPU/Windows runner；
- 与 core 的自动对接（目前是 HTTP 契约 + live 测试）。
