# 工程分析：现在这套东西只有我们能构建

> 状态更新（2026-11-16 晚）：P0/P1 已全部完成，P2 的 strip 也做了。
> 详见文末「已完成」。

## 一、查到的现状

| 项 | 事实 |
|---|---|
| 构建方式 | `X:\dsh_ws\harness-v2` → rsync → `~/ui`（ext4）→ `cargo build` |
| 路径 | **硬编码**：`sync.sh`/`test.sh`/`dev.sh` 里写着 `/mnt/x/dsh_ws/harness-v2` 和 `$HOME/ui` |
| 二进制 | 15 MB，**未 strip**，4045 个符号 |
| 版本 | `0.1.0` 写在 3 个 Cargo.toml 里；**无 tag、无 VERSION、无 `--version` 参数** |
| 门禁依赖 | python3（有）、node（**不在 PATH**）、mingw（**本地解压到 `~/mingw64`**） |
| CI / 发布 | **无** |
| 构建缓存 | `~/ui/target` 452M + `~/ui-target` 418M |

## 二、八个问题，按"会不会咬人"排序

### 1. 构建脚本只有我们能跑（最严重）

`tools/sync.sh` 写死源路径，`dev.sh`/`test.sh` 写死构建目录，Windows 交叉编译依赖
`~/mingw64` 这个我手工解压的目录。**别人 clone 下来，`bash tools/test.sh` 直接失败。**

### 2. 没有版本接口（最便宜）

- `ui --version` 打印帮助，忽略参数；
- `ui version` 是未知命令；
- 没有 git tag、没有 `VERSION` 文件、没有构建校验和。

外部基准的黄金任务就是"**给 CLI 加一个 `--version`**"。我们连这个都没有。

### 3. 二进制 15 MB 未 strip

`file` 显示 `not stripped`，`nm` 有 4045 个符号。

### 4. 门禁的依赖是隐式的

36 项里有的需要 python3、有的需要 node、有的需要 mingw、有的需要端口。
缺依赖时有的跳过、有的失败，没有一处统一声明。

### 5. 测试用固定端口

`7802/7803/7799/7806/7787` 写死，已经因为端口残留踩到过一次**偶发红**。

### 6. 工具链引导不可复现

`install_rustup_mirror.sh` / `get_mingw.sh` 后来删了，只剩假设工具链已就绪的
`build_windows.sh`。

### 7. 密钥卫生只有"排除"，没有"扫描"

`out/` 里出现过 `providers.toml`（假 key，但形状对）。

### 8. 没有故障上报包

用户报错时还得手动复制 doctor 输出 + 版本，且要人工脱敏。

## 三、建议的动手顺序（成本已核对）

| 优先 | 做什么 | 成本 | 状态 |
|---|---|---|---|
| **P0** | 构建路径参数化 + `bootstrap.sh` | 1h | ✅ 已完成 |
| **P0** | `--version` + 单一版本来源 + tag | 1h | ✅ 已完成 |
| **P1** | `preflight.sh` 声明依赖 | 1h | ✅ 已完成 |
| **P1** | 端口动态分配 | 1h | ✅ 已完成 |
| **P2** | `strip = true` | 20min | ✅ 已完成（15M → 1.9M） |
| **P2** | Windows 工具链引导 | 1h | ✅ `bootstrap.sh` 里有命令（未在干净机器验证） |
| **P2** | 密钥扫描 | 30min | ❌ 未做 |
| **P3** | `ui bug-report` | 2h | ❌ 未做 |

## 四、明确不做的

- **Docker 镜像**：卖点是"单二进制、零运行时"，容器化反而削弱它；
- **多平台 CI 矩阵**：只有 Linux 和 Windows 两个目标，手工足够；
- **brew/scoop 分发**：在还没有用户之前是空转。

## 五、已完成的部分（实测）

```bash
$ ui --version
ui 0.1.0 (commit c2ef25f, built 2026-11-16)

$ ls -lh ui
1.9M          # 原 15M，strip 后 -87%

$ cd /tmp && bash /mnt/x/.../tools/test.sh
== 测试通过 ==     # 不再依赖当前目录与写死路径

$ bash tools/preflight.sh
前置检查（缺什么会明说，不静默跳过）
  cargo      有    —
  python3    有    —
  sklearn    有    —
  mingw      有    —
结论：依赖齐全，36 项门禁可以全跑。
```

改法：
1. `tools/env.sh` 统一探测 `UI_ROOT`（按脚本位置）、`UI_BUILD_DIR`、`UI_TARGET_DIR`、
   `CARGO_TARGET_DIR`，并提供 `ui_free_port`；
2. 81 个脚本改为 `source env.sh` + `cd "$UI_ROOT"`，不再写死 `/mnt/x/...`；
3. `ui-cli/build.rs` 把 commit（含 `-dirty` 标记）与构建日期编进二进制，
   并监听 `.git/refs` 与 `index`，commit 变了会重新编译；
4. `strip = true`；
5. 5 个起服务的脚本改用 `ui_free_port`。

## 六、还差的

- **CI**（没有远端仓库，先做本地 `tools/ci.sh` 等价物？）；
- **发布包**：`dist/ui-0.1.0-linux-x86_64.tar.gz` + `.sha256`；
- **密钥扫描**、**bug-report**。

## 七、一句判断

修完之后，"别人无法复现我们的构建"这条已经解决：路径、版本、依赖、端口全部显式化。
剩下的 CI 与发布包是**分发**问题，不再是**可构建性**问题。
