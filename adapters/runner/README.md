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

请求体上限 1 MiB。`Content-Length` 必须存在、唯一、ASCII 十进制且非负；缺失、
非数字、负数一律 `400`，超过上限 `413`。`Transfer-Encoding: chunked`（以及其它
任何 Transfer-Encoding）一律 `411`——runner 只接受定长 JSON，不做分块解码。

## 隔离

- 只挂载 workspace 到 `/workspace:rw`；
- 在 workspace 挂载之后，用**匿名空卷**覆盖 `/workspace/secrets`、`/workspace/.ui`、
  `/workspace/.git`（`volume-nocopy`，宿主内容不会被复制进卷）；根目录存在 `.env`
  时用 `/dev/null` 只读 bind 覆盖，读到的永远是空内容；
- 上述路径不存在时不做遮蔽挂载：给不存在的路径挂空卷会让 Docker 在宿主
  workspace 里创建 root 所有的空目录，反而污染工作区；路径不存在时也没有内容可泄漏；
- rootfs `--read-only`；
- `/tmp` 为 `noexec,nosuid` tmpfs；
- `--network none`；
- `--memory 256m --cpus 1 --pids-limit 128`；
- `--cap-drop ALL --security-opt no-new-privileges`；
- 宿主 uid/gid 运行业务进程（rootless Docker 下 workspace 可写）；
- 超时 `docker kill`；
- stdout/stderr 各 8 MiB 上限，流式读取，超限立即停写并 kill。

### 容器内 shell 不受 Rust 工具层策略约束

`ui-core` / `protected.rs` 的路径保护只作用于**宿主上的工具层**。runner 执行的是
`sh -c <command>`，容器里的进程不会经过 Rust 工具注册表、路径校验或读取例外检查。
所以不能依赖容器内进程「自觉遵守」`secrets/`、`.ui/`、`.git/` 的保护；唯一可靠的
办法是在挂载层把这些路径遮蔽掉（上面的空卷 / `/dev/null`）。同理，容器内 shell
能做什么只由 Docker 的挂载、网络、资源限制和镜像本身决定，不由 core 的策略决定。

如果 Docker 不接受 `/dev/null` bind（少数 rootless/远程运行时），`docker run` 会
直接失败而不是降级放行；三个目录的遮蔽仍由空卷保证，根 `.env` 需要移出 workspace
或换用支持 device bind 的运行时。

## 输出

`runner.py` 用 `Popen` + 两条读取线程流式消费 stdout/stderr，每路最多保留
`MAX_OUTPUT`（8 MiB）。达到上限时立即停止追加并 `docker kill`，响应里
`truncated=true`。执行期不再写宿主 `TemporaryFile`，所以不存在「先无限落盘、
结束后才截断」的窗口。

## 测试

```bash
bash tools/verify_runner_live.sh
```

覆盖：workspace 可写、默认无网络、timeout、token、路径越界、`secrets`/`.ui`/`.git`/
`.env` 遮蔽、负数/缺失/非数字 Content-Length、chunked 拒绝、无限输出在 8 MiB 处截断。

实测：

```text
PASS runner live: workspace/network/timeout/token/escape/mask/body-validation/output-cap
```

## 未实现

- mTLS；
- seccomp/AppArmor 自定义 profile；
- 每任务独立网络 allowlist；
- GPU/Windows runner；
- 与 core 的自动对接（目前是 HTTP 契约 + live 测试）。
