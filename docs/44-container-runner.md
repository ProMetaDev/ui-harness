# M3：容器 runner（L2）

> 实现：[adapters/runner](adapters/runner/README.md)
>
> 目标：把工具执行从“宿主进程”推进到“容器 + 资源/网络边界”。

## 1. 已实现

- 本地 HTTP runner（Python 标准库）；
- `POST /execute` + Bearer token；
- workspace root 校验（realpath 越界拒绝）；
- HTTP 请求校验：
  - `Content-Length` 必须存在、唯一、ASCII 十进制且非负；缺失/非数字/负数 400；
  - 超过 1 MiB 上限 413；
  - `Transfer-Encoding: chunked` 及其它 TE 一律 411（只接受定长 JSON）；
- Docker：
  - 只挂 workspace；
  - workspace 挂载之后，用匿名空卷（`volume-nocopy`）遮蔽
    `/workspace/secrets`、`/workspace/.ui`、`/workspace/.git`；
  - 根目录存在 `.env` 时用 `/dev/null` 只读 bind 覆盖；
  - rootfs read-only；
  - `/tmp` noexec tmpfs；
  - `--network none`；
  - memory/cpu/pids 限制；
  - `cap-drop ALL` / `no-new-privileges`；
  - 宿主 uid/gid；
- 超时 `docker kill`；
- 输出流控：`Popen` + 读取线程边读边计数，stdout/stderr 各 8 MiB 上限，
  超限立即停止追加并 kill，`truncated=true`；执行期不写宿主临时文件；
- live 测试覆盖：workspace、network、timeout、token、路径越界、挂载遮蔽、
  负数/缺失/非数字 Content-Length、chunked 拒绝、无限输出截断。

## 2. 实测

```text
PASS runner live: workspace/network/timeout/token/escape/mask/body-validation/output-cap
```

## 3. 为什么必须在挂载层遮蔽

**容器内 shell 不受 Rust 工具层策略约束。** `ui-core` 的路径保护、读取例外、
`protected.rs` 的规则都只在宿主工具层生效；runner 交给容器的是 `sh -c <command>`，
它不会经过工具注册表，也不会调用 core 的路径校验。因此：

- 不能假设容器里的命令会遵守 `secrets/`、`.ui/`、`.git/` 的保护；
- 不能只在文档里写“不要读这些目录”；
- 唯一可靠的边界是 Docker 挂载：先把 workspace bind 到 `/workspace`，再在
  `/workspace/secrets` 等路径上盖匿名空卷，宿主内容根本不在容器命名空间里。

这与 L2 的定位一致：容器边界由 Docker 提供，不把安全责任下放给容器内的进程。

## 4. 边界

- 遮蔽只在 workspace 根目录的三个目录和根 `.env` 上做；更深层的自定义敏感文件
  需要由部署方决定是否加挂载。
- 上述路径不存在时不做遮蔽挂载：给不存在的路径挂空卷会让 Docker 在宿主
  workspace 里创建 root 所有的空目录，反而污染工作区；路径不存在时也没有内容可泄漏。
- `.env` 的 `/dev/null` bind 依赖运行时允许 bind device。若某些 rootless/远程
  Docker 不允许，`docker run` 会失败而不是降级放行；三个目录的遮蔽仍由空卷保证，
  此时应把根 `.env` 移出 workspace 或换用支持 device bind 的运行时。

未实现：

- mTLS；
- seccomp/AppArmor；
- 网络 allowlist；
- GPU/Windows runner；
- 与 core 自动对接；
- 多租户配额。

## 5. 为什么是 L2

Docker 是显式企业依赖；不进入 `ui-core`。core 继续 std-only。
