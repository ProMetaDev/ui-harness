# UI

一个只认七条契约的 harness。零依赖 Rust，单二进制，约 4700 行（含测试）。

> 起因：对 DSH 的解包诊断见 [docs/01-diagnosis.md](docs/01-diagnosis.md)。
> 305 个包、55 个 UI 插件、5 代会话格式之后，harness 缺的不是能力，是一根脊梁。

## 名字

**UI = Unleash your imagination。**

代价是它和 "user interface" 撞词，所以本仓库的写法约定：

- `UI` / `ui` / `ui-core` / `ui-cli` 指这个 harness 本身；
- 界面层一律叫**前端**，再细分 TUI / GUI / Web——不写"UI 的 UI"；
- 日志、状态里出现的是 `ui`（`rust (ui 0.1.0)`、`demo/.ui/journal.jsonl`）。

## 状态

可运行、有测试（**117 个**，含真实 SIGKILL 后的恢复、父死子停、跨轮历史、隔离回写与漂移拒绝、
21 个对抗性安全用例）。**已接通真模型**（DeepSeek，经 `tools/deepseek.mjs` 子进程适配器），
模型回复**流式**输出到网页端。

```bash
bash tools/debug.sh       # 一条命令验证全部能力（14 项分层检查）
bash tools/test.sh        # 只跑测试（失败会返回非零）
bash tools/eval.sh --task demo/task.txt --workspace fixtures/eval-workspace \
  --mock fixtures/scenarios/fix-bug.json --runs 3   # 重复跑，出 tokens/步数/耗时中位数
bash tools/chat_real.sh   # 真模型多轮：看问题 → 按方案改 → 重跑验证
ui serve --port 7788      # 网页端：浏览器里直接和模型对话
```

- 测试地图：[docs/05-tests.md](docs/05-tests.md)
- 成本分析（钱花在哪、什么杠杆有效）：[docs/06-cost.md](docs/06-cost.md)
- 安全边界（四道闸 + 已修/未修清单）：[docs/07-security.md](docs/07-security.md)

为什么是子进程而不是 Rust 里做 HTTP：内核零依赖、没有 TLS。适配器只负责传输，
决策、预算、策略、日志全在 Rust 侧——换模型只需换一个命令行。

## 快速开始（WSL）

```bash
cd /mnt/x/dsh_ws/harness-v2
export CARGO_TARGET_DIR=$HOME/ui-target
cargo test --offline
cargo build --release --offline

# 它对自己的认知（由真实进程探测，不是模板）
./target/release/ui doctor --workspace demo

# 跑一轮：读文件 → 改 → 再读确认 → 收尾
./target/release/ui run --task-file demo/task.txt \
  --mock fixtures/scenarios/fix-bug.json --workspace demo --yes

# 回放：每一步的快照、计价、决策、副作用
./target/release/ui replay --workspace demo

# 交互式会话（多轮，同一份日志）
./target/release/ui chat --mock fixtures/scenarios/fix-bug.json --workspace demo
```

一次真实运行的尾部：

```
scripted-demo │ tok 1719/200000 ········ │ calls 3/60 │ 0s │ $0.000
结束原因 done · 3 步 · 3 次工具 · 约 1719 tokens
```

## 七条契约

| # | 契约 | 机制 | 代码 |
|---|---|---|---|
| 1 | 一步一快照 | 分段、标价、哈希、落盘的不可变 `Snapshot` | `prompt.rs` |
| 2 | 一调用一效果 | 工具按**这一次的参数**算出效果集 | `tools.rs` |
| 3 | 一效果一决策 | 数据化规则，最长字面前缀胜出 | `policy.rs` |
| 4 | 一预算一账户 | 脚本感知分词 + 硬上限，超支是错误 | `budget.rs` |
| 5 | 一副作用一日志 | 先写日志再执行，哈希链，撕裂尾部可修 | `journal.rs` |
| 6 | 一委派一邮箱 | 未实现（v0.2，见设计文档） | — |
| 7 | 一错误一修复 | 参数 JSON 损坏 → 抢救 → 修复回合 → 结构化失败 | `agent.rs` |

## 命令

```
ui doctor                 报告对这台机器的真实认知（含探测结论）
ui run --task-file F      跑一轮
ui tui                    全屏对话（Enter 发送 · Esc 退出 · PgUp/PgDn 滚动 · y/a/n 审批）
ui chat                   行式会话（管道/脚本可用）
ui preview --cols N --rows M  渲染一帧界面（不需要终端）
ui resume                 对账崩溃打断的副作用
ui replay                 回放日志
ui export --html F        导出单文件轨迹查看器（双击即看，离线可转发）
ui skills                 列出工作区里的 skill
ui workflow --file F      按文件里的步骤跑工作流（每步一次 turn，可设闸门）
ui workflow --file F --isolate   并行子代理各用一份工作区副本
ui apply --id ID [--yes]  把某个隔离子代理的改动写回（--all；冲突/漂移返回 3，--force 强覆盖）
ui verify                 日志自检：哈希链、未完成调用
ui policy --effect K --target T   解释一个决策
```

通用参数：`--workspace PATH`、`--task-file PATH`、`--max-tokens`、
`--max-calls`、`--max-wall-ms`、`--max-usd`、`--yes`、
`--compact-tokens N`（0 = 关闭折叠）、`--keep-recent K`、
`--skills a,b`（开场前预加载 skill）、`--journal PATH`（并行子代理各写各的日志）、
`--result-file PATH`（把最终答案写进文件，供父进程读取）、
`--isolate`（子代理用工作区副本）、`--force`（忽略漂移强制回写）、
`--json`（机器可读输出；`apply` 用它时退出码 3=冲突、4=漂移）。

## 为什么在 WSL 里构建

同一台机器、零依赖项目实测（真的改一行代码，不是 `touch`）：

| | 源码在 `/mnt/x`（9p） | 源码在 WSL ext4 |
|---|---|---|
| 干净构建 | 19.6 s | **3.1 s** |
| 增量（改一行） | 3.4 s | **2.0 s** |

结论要分情况：**干净构建差 6 倍**（cargo 要扫整个源码树 + 写 `target`），
**增量只差 1.7 倍**（大头是 rustc 本身，跟文件系统关系不大）。
所以真正必须守住的只有一条：**`target` 不要放在 `/mnt`**。

- 日常 `cargo build/test` 在 `/mnt` 上也能忍（3.4 s）；
- 干净构建、加依赖、CI 走快路径：`bash tools/dev.sh test`（先同步到 `~/ui` 再编译）；
- `/mnt/x` 是唯一真源；`~/ui` 是只读构建副本，`secrets/` 不参与同步；
- 编辑器/rust-analyzer 建议打开 `~/ui`，否则索引同样慢。

Windows 原生 cargo 不用考虑：本项目的 TUI 依赖 `stty` 做 raw mode，是 POSIX 实现。

Windows 原生 cargo 不用考虑：本项目的 TUI 依赖 `stty` 做 raw mode，是 POSIX 实现。

## 布局

```
ui-core/     内核：json sha256 time journal budget policy prompt tools model agent
ui-cli/      单二进制：doctor | run | chat | resume | replay | policy
fixtures/      剧本（确定性回放，测试与演示共用）
demo/          演示工作区
tools/         演示脚本
docs/          设计文档
```

## 明确的缺口

- **没有真实模型**：`model.rs` 里 `ExternalModel` 是给外部进程（未来的 TS 侧
  或带 TLS 的 helper）用的缝，`ScriptedModel` 是回放。HTTP+TLS 适配器没写。
- **没有流式**：模型响应是一次性的，所以 TUI 是整段刷新而非逐字。
- **TUI 还没有颜色**：帧是纯文本，256 色配色方案（见 `docs/03-frontend.md`）尚未接。
  右栏（快照分段 + 决策时间线）也还没做，当前只有单栏 + 底部状态行。
- **没有委派**：契约 6 只有设计，没有代码。
- **没有压缩**：历史未超预算，压测待做。
- **无网络/进程沙箱**：策略层能表达 `net.fetch` / `proc.exec`，但执行侧只做了
  文件效果；进程执行是真实执行的，靠 `ask` 拦。

## 许可

内部原型，未定。
