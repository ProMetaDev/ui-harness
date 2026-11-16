# 测试地图

一条命令验证全部能力：

```bash
bash tools/debug.sh      # 23 项分层检查（内核 / 帧 / 终端 / 功能闭环）
bash tools/test.sh       # 全部单元与集成测试（数量以输出为准）
```

任何一层 FAIL 就停在那层——上面的层不值得看。

## 契约 → 测试

| 契约 / 能力 | 测试位置 | 覆盖点 |
|---|---|---|
| 1 一步一快照 | `ui-core/src/prompt.rs::tests` | 分段计价、同输入同哈希、缺工作区要告警、段级 diff |
| 1 成本控制 | `ui-core/src/agent.rs::tests` | 长工具输出落盘、上下文只留首尾+指针、短输出不动、`tool/trim` 留痕 |
| 1 上下文折叠 | `ui-core/src/compact.rs::tests` | 阈值内不动、保锚点+窗口、失败计数、用户事实、模型摘要、**失败回落**、负收益拒绝 |
| 2/3 效果与决策 | `ui-core/src/policy.rs::tests` | glob 语义、最长前缀、workspace 根、密钥拒绝、未知效果默认拒绝 |
| 4 预算 | `ui-core/src/budget.rs::tests` | CJK 计价高于 ASCII、超限报错、provider usage 对账 |
| 5 一副作用一日志 | `ui-core/src/journal.rs::tests` | 哈希链、中间篡改拒绝、**尾部撕裂可修** |
| 6 委派 | `ui-cli/tests/workflow.rs` | 并行组、独立日志、**父死子停**、隔离副本、apply、冲突拒绝、**漂移拒绝**、`--force`、`--json` |
| 7 一错误一修复 | `ui-core/src/agent.rs::tests` | 参数抢救、**协议违规要求重写**、**空回复不算完成** |
| 取消 | `ui-core/src/cancel.rs` + `agent.rs` | 普通取消可复位、硬取消不可复位、跨轮不丢 |
| 模型适配 | `ui-core/src/model.rs::tests` | 围栏剥离、截断抢救、彻底失败要报错 |
| 工具 | `ui-core/src/tools.rs::tests` | 读写编辑、歧义拒绝、shell 退出码 |
| 帧渲染 | `ui-cli/src/frame.rs::tests` | 每行恰好 N 列、CJK 双宽、换行切分、样式归属、浮层完整 |
| 右栏「当前步」 | `ui-cli/src/frame.rs::tests` | ≥100 列出现 / 99 列消失、快照分段、决策时间线、放不下时明说「更早 N 条」、账本贴底 |
| 终端尺寸 | `tools/tui_smoke.sh` | 伪 tty 设成 120×30，右栏必须真的出现（抓 `stty size` 读不到 tty 的回退） |
| 终端按键 | `ui-cli/src/term.rs::tests` | 方向键/翻页转义、UTF-8 多字节、按键队列 |
| 行式会话 | `ui-cli/tests/chat.rs` | 两轮共享一份日志 |
| 崩溃恢复 | `ui-cli/tests/crash_resume.rs` | 真 SIGKILL 后日志仍可验证、resume 对账 |
| CLI 不变量 | `ui-cli/tests/cli.rs` | `--journal` 改道、损坏返回 2、干净返回 0 |
| 导出 | `ui-cli/tests/export.rs` | 占位符替换、事件内嵌、限值随参数、无外链 |

## L4 功能闭环（`tools/debug.sh` 里跑）

| 检查 | 证明什么 |
|---|---|
| `skills 列表` | 目录发现 + 描述解析 |
| `workflow --json` | 结果单行 JSON、进度走 stderr |
| `pty 全屏冒烟` | 备用屏进出、输入发送、审批键、**120 列右栏** |
| `web 端（HTTP + SSE）` | 页面可取、`/api/state`、发消息、SSE 收到 user/assistant/tool/turn |
| `gate 未批准则停` | 闸门返回 1 且不产生 turn |
| `隔离 + apply + 漂移` | 副本不污染父工作区、apply 回写、**再 apply 判漂移** |
| `导出查看器并校验` | 内嵌 JSON 可解析、无外链、包含快照字段 |

## 评测（判断改动有没有效果）

同一个任务重复跑 N 次，每次一份干净工作区，输出表格和 JSON：

```bash
bash tools/eval.sh --task demo/task.txt \
  --workspace fixtures/eval-workspace \
  --mock fixtures/scenarios/fix-bug.json \
  --runs 5 --label baseline

# 改完（例如换 --tool-chars）再跑一次，对比 tokens / steps / wall_s
bash tools/eval.sh --task demo/task.txt \
  --workspace fixtures/eval-workspace \
  --mock fixtures/scenarios/fix-bug.json \
  --runs 5 --label after --tool-chars 2000
```

- `--workspace` 只是模板：每次复制到 `out/eval/<label>-<时间戳>/run-N/workspace`，
  原目录不会被写；不传时用 `fixtures/eval-workspace`。
- 统计写入 `out/eval/<label>-<时间戳>.json`，含 `runs` 和
  `summary.<指标>.median/min/max`（`tokens` / `steps` / `calls` / `wall_ms`）。
- 任何一次 `结束原因 != done`（或 ui 退出码非 0）→ 脚本退出 1，可以直接当门禁。
- `--mock` 必填，评测不会调用真实模型。

自测（3 次 mock，退出 0）：

```bash
bash tools/eval.sh --task demo/task.txt --runs 3 \
  --mock fixtures/scenarios/fix-bug.json --label selftest
```

负向门禁自测（应退出 1）：

```bash
bash tools/eval.sh --task demo/task.txt --runs 1 \
  --mock fixtures/scenarios/eval-not-done.json --label should-fail
```

## 几条自己踩出来的规矩

1. **失败必须是非 0 的**：空回复、协议违规、闸门拒绝、漂移，全都不能以 `done` 结束。
2. **断言优于自述**：子代理说"已写入"不算，文件系统比对才算（`changed`）。
3. **不猜**：冲突不同内容就跳过，漂移就跳过，宁可让人来合。
4. **每个修过的 bug 都要留下一个测试**，否则它会以另一种形状回来。
