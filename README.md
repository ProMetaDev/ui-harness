# ui

[![ci](https://github.com/ProMetaDev/ui-harness/actions/workflows/ci.yml/badge.svg)](https://github.com/ProMetaDev/ui-harness/actions/workflows/ci.yml)

**Unleash your imagination。**

ui 是一个用 Rust 写的 agent harness：内核零第三方依赖，单二进制 1.9 MB（strip 后），
主张“每一步都能解释”——把每一步工具调用写成追加式 JSONL 日志，再用 SHA-256 哈希链串起来，
可校验、可回放、可崩溃对账。

当前 v0.1.1：**38 项门禁、132 个测试**、13 个工具，支持 MCP 与本地插件。
CI（GitHub Actions）在干净 checkout 上跑同一套：preflight → 构建 → 版本/密钥 → 测试 → 门禁 → 发布包。

## 30 秒上手

需要 Rust 工具链；下面的脚本在 bash（WSL / Linux）里执行。

```bash
bash tools/bootstrap.sh   # 检查依赖：缺什么、怎么装，逐条说明
bash tools/test.sh        # 同步构建副本并跑 132 个测试
bash tools/debug.sh       # 38 项门禁：release 构建 + 端到端 + 7 个攻击场景
```

构建产物在 `$UI_BIN`（由 `tools/env.sh` 定义，默认 `$HOME/ui-target/release/ui`）；
首次执行 `debug.sh` 会先做 release 构建，耗时以本机为准。
版本用 `ui --version` 查看，完整命令清单用 `ui --help`（不带参数同样会打印）。

## 它和别的 harness 有什么不同

1. **日志是执行记录，不是调试输出。** 先写日志再执行；每行带 `prev` / `hash`，
   哈希链可以独立校验。崩溃后用 `verify` / `resume` / `replay` 对账和回放。

2. **严格度与日志格式解耦。** `quiet` / `standard` / `strict` 只调整
   Ask/Allow 的判定，不改日志格式；所以历史可以用另一个模式重判：
   `ui audit --as strict` 会列出判定变严的动作。

3. **验证材料可以自己跑、自己破坏。** `tools/kit.sh` 的 7 个攻击场景会给出
   `BLOCKED` / `REPAIRED` / `GAP` 三种结论，包括承认现在拦不住的；
   `tools/verify_journal.py` 是独立的 Python 校验器，不依赖 Rust 实现。

## 主要命令

| 命令 | 作用 |
|---|---|
| `ui doctor` | 探测这台机器的真实能力，并打印信任边界 |
| `ui run --task-file F` | 跑一轮任务（可用 `--mock` 剧本） |
| `ui chat` | 行式多轮会话，始终同一份日志 |
| `ui tui` | 全屏终端界面 |
| `ui serve --port 7788` | 本地网页端 |
| `ui replay` | 回放日志：每一步的快照、计价、决策、副作用 |
| `ui verify` | 校验哈希链与未完成调用 |
| `ui resume` | 对账崩溃打断的副作用 |
| `ui audit --as strict` | 用严格模式重判历史 |
| `ui policy --effect K --target T` | 解释一条效果为什么被放行或拒绝 |

命令表之外还有 `skills` / `workflow` / `team` / `apply` / `export` / `lab` / `preview` 等。
通用参数：`--workspace PATH`、`--max-tokens`、`--max-calls`、`--max-wall-ms`、
`--max-usd`、`--yes`、`--mode quiet|standard|strict`、`--mcp mcp.json`。

## 诚实的边界

- **shell 是信任边界。** 批准一条命令，等于批准它内部的一切读写与联网；
  效果策略只决定“要不要执行这条命令”，不约束命令内部做什么。
- **插件声明不是强制约束。** `effects: []` 只表示它声称没有副作用，内核不会验证；
  声明之外的写入可以发生，也不会自动出现在日志里。
- **MCP 同理。** 外部 server 在自己的进程里自主运行，工具效果记为
  `proc.exec mcp:<server>:<tool>`，内部行为不受效果策略约束。
- 目前没有 OS 级网络/进程沙箱。`proc.exec` 是真执行，靠 `ask` 拦。
  请把 shell / 插件 / MCP 当“可执行程序”，不是“沙箱里的受限代码”。

## 验证方式

```bash
bash tools/kit.sh                        # 7 个攻击场景，输出一张结论表
python3 tools/verify_journal.py JOURNAL  # 0 完好 / 2 链断或被改 / 3 尾部残缺
```

- 场景说明、日志格式与复跑记录模板：[docs/22-verify-kit.md](docs/22-verify-kit.md)。
  格式只有两条规则，任何语言约 20 行就能独立实现。
- 如果你跑出表外的结论，那比表内全部通过更有价值。

## 状态与许可

- 原型，v0.1.1。CI 在 Linux 上全绿（38/38）。能运行、有测试，也有**可校验的发布包**：
  `bash tools/release.sh` 会构建、打包、算 sha256 并自校验（解压重算）。
  仍然**没有 CI**——改动靠手跑门禁。
- Windows 已在真机验证（含 `serve` 与真模型），但**临时目录不可写**：
  `doctor` 会告警，相关工具退到工作区写临时文件。
- 本文数字（38 项门禁、132 个测试、13 个工具、1.9 MB）来自本仓库自己的测量，
  没有第三方独立评审。
- 许可证：Apache-2.0。
