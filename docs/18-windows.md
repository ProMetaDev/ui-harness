# Windows 实机验证：找到 4 个只有真机才会暴露的问题

背景：自评和 T1–T7 都是在 Linux（WSL）上跑的。Windows 从来没跑过，
所以本机 `doctor` 里 H3（跨平台）一直只给 1 分。这次真的编出 `ui.exe` 跑了一遍。

## 怎么做到的（不需要 root、不动 Windows）

```bash
# 1. 用国内镜像装 rustup + Windows 目标
RUSTUP_DIST_SERVER=https://rsproxy.cn rustup-init -y --default-toolchain stable
rustup target add x86_64-pc-windows-gnu

# 2. 没有 mingw 链接器、sudo 又要密码 → apt-get download 到本地解压
apt-get download gcc-mingw-w64-x86-64 binutils-mingw-w64-x86-64 mingw-w64-x86-64-dev ...
dpkg-deb -x *.deb ~/mingw64/root

# 3. 用本地 mingw 链接出 ui.exe
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=~/mingw64/root/usr/bin/x86_64-w64-mingw32-gcc-posix \
  cargo build --release --target x86_64-pc-windows-gnu

# 4. 拷到 X: 盘，用 PowerShell 直接跑
X:\dsh_ws\harness-v2\out\win\ui.exe doctor
```

## 找到的四个问题

| # | 问题 | 现象 | 修法 |
|---|---|---|---|
| 1 | **JSON 带 BOM 无法解析** | `剧本不是合法 JSON: unexpected 'ï' at byte 0` | `json::parse` 先剥 `\u{feff}` |
| 2 | **源码文件带 BOM+CRLF 时 patch 失败** | `@@ 块在原文里找不到对应内容` | 剥 BOM 再匹配、CRLF 归一化后按原风格还原 |
| 3 | **Windows 没有 GNU grep** | `没有可用的 grep：program not found` | 退化为内置字面匹配，并在结果里注明 |
| 4 | **临时目录不可写** | `拒绝访问 (os error 5)` | **未修**：`doctor` 如实告警，工具改把临时文件写进工作区 |

前三个是**功能 bug**：Windows 用户第一次用就会撞上（PowerShell 的 `Set-Content`
默认写 UTF-8 BOM，编辑器默认 CRLF）。第四个更像环境/策略问题（PowerShell 自己
能写同一个目录），暂时按"真实能力"如实报告，不假装正常。

另外修了一个显示问题：`canonicalize` 会加 `\\?\` 前缀，
日志里变成 `\\?\X:\...`，现在统一剥掉。

## Windows 上确实能用的（实机验证）

| 能力 | 结果 |
|---|---|
| `doctor` | 正确识别 windows x86_64、PowerShell 方言、临时目录告警 |
| `run --mock` | 完整一轮：fs_write 写文件、shell 跑 `echo %CD%` 正确 |
| `verify` | 哈希链完整、事件计数正确 |
| shell 超时 | 800ms 超时在 1s 内结束，**无残留 ping 进程**（走 taskkill） |
| tree / fs_mv | 正常 |
| patch（BOM+CRLF） | 修复后正常，且保留 BOM |
| grep | 无 grep 时退化为字面匹配并明确告知 |

## 还没验证的

- **真模型**在 Windows 上跑（只跑了脚本化模型）；
- `serve` 网页端在 Windows 上（端口/进程/token 全链路）；
- 中文路径、长路径（>260 字符）、UNC 路径；
- 临时目录不可写的**根因**（可能是 Defender 的受控文件夹访问）。

## 第二批：真模型 + 网页端（同一天）

### 又找到两个 Windows 专属 bug

| # | 问题 | 现象 | 修法 |
|---|---|---|---|
| 5 | **`cmd /C` 把引号参数转义坏** | `'"X:\...\node.exe"' is not recognized` | 适配器命令在 Windows 上拆成 argv 直接起进程（`split_command_line`），只有含 `| > & <` 时才退回 cmd |
| 6 | **中文控制台乱码** | 启动横幅变成 `UI 宸插惎鍔?` | 启动时 `SetConsoleOutputCP(65001)` + `SetConsoleCP(65001)` |

第 5 条踩了两次：先试 `arg()`，再试 `raw_arg()`，都不行——`cmd.exe` 对
"以引号开头的命令"有去引号的历史行为。最后绕开 shell 才彻底解决。

第 6 条还有个**假象**：把 stdout 重定向到文件后，PowerShell 用 GBK 读回来仍是乱码，
看着像程序没修好。用 `-Encoding UTF8` 读就正常——**文件本身是对的**。

### 实机验证通过的能力

| 能力 | 结果 |
|---|---|
| **真模型**（DeepSeek 适配器） | 1234 tok / 2s，回答正常 |
| **serve 网页端** | 401 无 token / 303 种 Cookie / API 200 / SSE 有 session 事件 |
| serve 的 `POST /api/say` | UTF-8 请求体返回 `{"ok":true}`（ASCII 那次是我的测试被 PowerShell 吃了引号） |
| console 中文 | 正常 |

### 仍未解决

- 临时目录不可写（`os error 5`）：doctor 如实告警，工具退到工作区；
- 中文路径 / 长路径 / UNC 路径没测；
- 浏览器里的完整交互（只验了 HTTP 层）。
