# 工程分析：现在这套东西只有我们能构建

## 一、查到的现状

| 项 | 事实 |
|---|---|
| 构建方式 | `X:\dsh_ws\harness-v2` → rsync → `~/ui`（ext4）→ `cargo build` |
| 路径 | **硬编码**：`timeout`/`sync.sh`/`test.sh`/`dev.sh` 里写着 `/mnt/x/dsh_ws/harness-v2` 和 `$HOME/ui` |
| 二进制 | 15 MB，**未 strip**，4045 个符号 |
| 版本 | `0.1.0` 写在 3 个 Cargo.toml 里；**无 tag、无 VERSION、无 `--version` 参数** |
| 门禁依赖 | python3（有）、node（**不在 PATH**）、mingw（**本地解压到 `~/mingw64`**） |
| CI / 发布 | **无** |
| 构建缓存 | `~/ui/target` 452M + `~/ui-target` 418M |

## 二、八个问题，按"会不会咬人"排序

### 1. 构建脚本只有我们能跑（最严重）

`tools/sync.sh` 写死源路径，`dev.sh`/`test.sh` 写死构建目录，Windows 交叉编译依赖
`~/mingw64` 这个我手工解压的目录。**别人 clone 下来，`bash tools/test.sh` 直接失败。**

爆炸形式：你说"跑一下 tools/test.sh"，对方报 `rsync: not found` 或路径不存在，
然后问"是不是要装什么"——而答案散落在 98 个脚本里。

### 2. 没有版本接口（最便宜）

- `ui --version` 打印帮助，忽略参数；
- `ui version` 是未知命令；
- 没有 git tag、没有 `VERSION` 文件、没有构建校验和。

讽刺的是：外部基准的黄金任务就是"**给 CLI 加一个 `--version`**"。我们连这个都没有。

爆炸形式：别人问"你测的是哪个版本？"，我们只能答"昨天那个提交"。

### 3. 二进制 15 MB 未 strip

`file` 显示 `not stripped`，`nm` 有 4045 个符号。开 `strip` 后预计 **3–4 MB**
（-75%）。传输、下载、镜像的成本都是它的几倍。

### 4. 门禁的依赖是隐式的

`tools/debug.sh` 的 36 项里，有的需要 python3、有的需要 node、有的需要 mingw、
有的需要端口。缺依赖时有的**跳过**（`bench_ml` 检查 sklearn 后 exit 0）、
有的**失败**（`verify_globs` 需要 python3）。没有一处统一声明。

爆炸形式：CI 上"36 项全绿"其实跳过了 5 项，没人知道。

### 5. 测试用固定端口

`7802/7803/7799/7806/7787` 写死在脚本里。我们已经因为端口残留踩到过一次**偶发红**。

爆炸形式：CI 并行跑两个 job，随机一个红；或者本机有服务占着端口。

### 6. 工具链引导不可复现

Windows 交叉编译需要：rustup（镜像）+ `x86_64-pc-windows-gnu` target + mingw 链接器。
我当时写的 `install_rustup_mirror.sh` / `get_mingw.sh` 后来删了，现在只剩
`build_windows.sh`——它假设工具链已就绪。

爆炸形式：换台机器，Windows 包做不出来，而"怎么做出来的"没留文档。

### 7. 密钥卫生只有"排除"，没有"扫描"

`out/` 里出现过 `providers.toml`（是假 key，但形状对）。当前防线只有
`.gitignore` 排除 `secrets/`、备份排除 `secrets/`——**没有主动扫描**。

爆炸形式：某次调试把真 key 写进 `out/xxx/`，进了截图或发给了别人。

### 8. 没有故障上报包

`ui doctor` 很好，但用户出问题时还得手动复制 doctor 输出 + 版本 + 日志片段，
而且**必须脱敏**（日志里有 prompt 原文，可能含密钥）。

爆炸形式：收到"报错了"，然后来回三轮才拿到有用的信息。

## 三、建议的动手顺序

| 优先 | 做什么 | 成本 | 完成标志 |
|---|---|---|---|
| **P0** | 构建路径参数化 + `tools/bootstrap.sh`（检查并说明缺什么） | 1h | 在干净目录里 `UI_SRC=... bash tools/test.sh` 能跑 |
| **P0** | `--version` + `VERSION` 单一来源 + git tag + 构建校验和 | 1h | `ui --version` 输出 `ui 0.1.0 (commit abc1234, built 2026-11-16)` |
| **P1** | `tools/preflight.sh`：声明 36 项检查各自依赖，缺什么就明说"将跳过 N 项" | 1h | 缺 node 时输出"skip 3 项"而不是静默 |
| **P1** | 端口动态分配（`--port 0` + 读回实际端口） | 1h | 并发跑两个 serve 不冲突 |
| **P2** | `strip = true` + 体积对比 | 20min | 15M → ~3.5M |
| **P2** | Windows 工具链引导脚本（恢复并文档化） | 1h | 新机器一条命令编出 `ui.exe` |
| **P2** | `tools/scan-secrets.sh`：扫描工作树里的密钥形状 | 30min | 命中 `sk-` 即失败 |
| **P3** | `ui bug-report`：version + doctor + 最近 N 条错误（脱敏）打成 tar.gz | 2h | 一条命令产出可粘贴的包 |

## 四、明确不做的

- **Docker 镜像**：我们的卖点是"单二进制、零运行时"，容器化反而削弱它；
- **多平台 CI 矩阵**：现在只有 Linux 和 Windows 两个目标，手工足够；
- **brew/scoop 分发**：在还没有用户之前是空转。

## 五、一句判断

功能面已经到 78%（自用）/45%（可交付），而**工程化只有 15%**——这 15% 里最要命的
不是"没有 CI"，而是**"别人无法复现我们的构建"**。先修这个，CI 才有意义。
