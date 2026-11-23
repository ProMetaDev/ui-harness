# M3：容器 runner（L2）

> 实现：[adapters/runner](adapters/runner/README.md)
>
> 目标：把工具执行从“宿主进程”推进到“容器 + 资源/网络边界”。

## 1. 已实现

- 本地 HTTP runner（Python 标准库）；
- `POST /execute` + Bearer token；
- workspace root 校验（realpath 越界拒绝）；
- Docker：
  - 只挂 workspace；
  - rootfs read-only；
  - `/tmp` noexec tmpfs；
  - `--network none`；
  - memory/cpu/pids 限制；
  - `cap-drop ALL` / `no-new-privileges`；
  - 宿主 uid/gid；
- 超时 `docker kill`；
- 输出 8 MiB 上限；
- live 测试覆盖：workspace、network、timeout、token、路径越界。

## 2. 实测

```text
PASS runner live: workspace/network/timeout/token/escape
```

## 3. 边界

未实现：

- mTLS；
- seccomp/AppArmor；
- 流式输出；
- 网络 allowlist；
- GPU/Windows runner；
- 与 core 自动对接；
- 多租户配额。

## 4. 为什么是 L2

Docker 是显式企业依赖；不进入 `ui-core`。core 继续 std-only。
