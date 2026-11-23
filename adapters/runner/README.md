# 容器 runner（M3，L2）

> 不属于 `ui-core`；需要 Docker。core 仍 std-only。

## 启动

`--token`（或环境变量 `RUNNER_TOKEN`）**必须非空**：没有 token 时 runner 直接报错退出
（退出码 2），不会以「不校验」的姿态起来。token 是 `/execute` 唯一的身份边界，
空 token 等于把任意命令执行开给本机所有进程。

```bash
export RUNNER_TOKEN="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
python3 adapters/runner/runner.py \
  --port 18089 \
  --workspace-root /srv/ui/workspaces \
  --image python:3.11-slim \
  --token "$RUNNER_TOKEN"
```

也可以用 `RUNNER_TOKEN=... python3 adapters/runner/runner.py ...` 而不传 `--token`；
命令行参数优先于环境变量。只给 `--token ""` 同样按缺失处理并拒绝启动。

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

`Authorization` 缺失或不匹配一律 `401`（服务端没有「无 token 放行」分支）。
`/healthz` 不需要 token：它只回 `{"ok":true}`，用于探活，不执行任何命令。

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
