# 真实付费 provider 的 1 轮 smoke

> 脚本：[tools/soak_paid_smoke.sh](tools/soak_paid_smoke.sh)
> 底层：[tools/soak_paid.sh](tools/soak_paid.sh) → [tools/soak_real.sh](tools/soak_real.sh)
> 长跑入口：[docs/53-paid-soak.md](53-paid-soak.md) · 真实长跑：[docs/50-real-soak.md](50-real-soak.md) · mock 长跑：[docs/48-soak.md](48-soak.md)

`tools/soak_paid_smoke.sh` 是 `tools/soak_paid.sh` 的**固定 1 轮包装器**。
它不复制轮次、工具判定、`ui verify` 或 state 逻辑，只做三件事：

1. 检查四道闸；缺任意一道就 SKIP，**不产生任何模型调用**；
2. 强制 `rounds=1`，把预算收紧到默认 `max_usd=0.01 / max_tokens=30000 /
   max_calls=20 / run timeout=300s`，然后调用 `tools/soak_paid.sh`；
3. 把结果汇总成 `smoke-report.json`（commit/model/phase/tokens/cost/state/log）。

它回答的问题只有一个：**这个 commit + 这个模型 + 这个端点，能不能跑完 1 轮
带工具的任务，并且日志链可校验？**

## 这会真实计费

只要脚本不是 SKIP，它就会启动真实 provider，按 token 计费。默认值很小，但
「小」不是「免费」：

- 真实端点按输入/输出 token 收费，价格由 provider 决定，脚本不猜；
- 预算触顶只会中止**后续**调用；已经发生的调用仍然会计费；
- 默认适配器 [tools/chat_compat.mjs](tools/chat_compat.mjs) 内部可能重试
  （默认 3 次），真实请求数可能高于脚本打印的调用下限；
- `smoke-report.json` 里的 tokens 和 cost 是估算/汇总，**不是账单或收据**。

## 为什么默认 SKIP

这个入口一旦误跑，代价不是多几行日志，而是真实账单。最常见的误跑是：

1. CI 或 shell profile 里恰好有 `OPENAI_API_KEY`，某个脚本顺手调用了它；
2. 想跑测试，却把 key 和 `UI_SOAK_ALLOW_COST` 一起带进了环境；
3. 模型/端点已经换到更贵的一档，但没人注意。

所以默认直接运行一定是 SKIP：

```bash
bash tools/soak_paid_smoke.sh
# SKIP paid-smoke：未进入真实计费运行（没有产生任何模型调用）
#   - 缺少 UI_PAID_API_KEY 或 OPENAI_API_KEY：没有付费凭据
#   ...
```

SKIP 也会写 `smoke-report.json`（`phase=skipped`、`tokens.total=0`、
`cost.usd=null`、`skipReasons` 列出缺的闸），让「没跑」和「跑失败」在文件层面
分得清。它**不会**调用 `tools/soak_paid.sh`，更不会调用模型。

## 四道闸

| 闸 | 变量 | 不满足时 |
|---|---|---|
| 凭据 | `UI_PAID_API_KEY` 或 `OPENAI_API_KEY` | SKIP；不读 `UI_CHAT_KEY` 当闸门 |
| 模型 | `UI_PAID_MODEL` | SKIP；不猜默认模型，避免跑错价格档 |
| 成本开关 | `UI_SOAK_ALLOW_COST=1` | SKIP；必须显式为 `1` |
| 人工确认 | `UI_SOAK_YES=1` | SKIP；本入口不读交互输入，非交互也必须显式确认 |

`UI_SOAK_YES=1` 不是成本开关，仍然需要 `UI_SOAK_ALLOW_COST=1`。本入口比
`tools/soak_paid.sh` 更严格：即使终端是交互式的，也只认 `UI_SOAK_YES=1`，
避免在管道/CI 里出现「没人看着却开始计费」的分支。

## 固定预算

轮数永远是 1，环境里的 `UI_SOAK_ROUNDS` 会被忽略（并打印一条提示）。预算默认：

| 项 | 默认 | 覆盖变量（推荐 / 兼容） |
|---|---|---|
| `rounds` | `1`（固定） | 不可覆盖 |
| `max_usd` | `0.01` | `UI_SMOKE_MAX_USD` / `UI_SOAK_MAX_USD` |
| `max_tokens` | `30000` | `UI_SMOKE_MAX_TOKENS` / `UI_SOAK_MAX_TOKENS` |
| `max_calls` | `20` | `UI_SMOKE_MAX_CALLS` / `UI_SOAK_MAX_CALLS` |
| 单轮 run 超时 | `300` 秒 | `UI_SMOKE_RUN_TIMEOUT` / `UI_SOAK_RUN_TIMEOUT` / `UI_PAID_RUN_TIMEOUT` |
| 每百万 token 美元价 | 未设置 | `UI_SMOKE_USD_PER_MTOK` / `UI_PAID_USD_PER_MTOK` / `UI_SOAK_USD_PER_MTOK` |

金额上限要说清楚：

- `ui --max-usd` 只有在同时给出 `--usd-per-mtok` 时才按金额中止；内核不猜价格。
- 只有设置了 `UI_SMOKE_USD_PER_MTOK`（或兼容变量）且 `> 0`，USD 硬中止才启用；
  否则 `--max-usd` 只是名义值，真正硬中止的是 `max_tokens` / `max_calls` / 墙钟。
- 预算非法（非数字、`max_usd <= 0`、`max_tokens/max_calls < 1`、超时不是非负整数）
  会在调用 `tools/soak_paid.sh` **之前**以 `exit 2` 退出，同样不产生调用。

运行前脚本会打印「这会真实计费」、模型、固定轮数、四项预算、USD 是否硬中止、
预计墙钟和产物路径，然后才调用底层脚本。

## 产物：smoke-report.json

默认路径 `${TMPDIR:-/tmp}/ui-paid-smoke-report.json`，原子写入，权限 `0600`。
可用 `UI_SMOKE_REPORT` 覆盖。主要字段：

| 字段 | 含义 |
|---|---|
| `commit` | 启动时 HEAD；worktree 的 `.git` 文件也能解析，失败才是 `unknown` |
| `model` | `UI_PAID_MODEL`（或底层 state 的 `modelLabel`） |
| `phase` | `passed` / `failed` / `skipped`，以 `tools/soak_paid.sh` 的 report/state 为准 |
| `exitCode` | 本次 smoke 的退出码（0/1/2/3/130） |
| `rounds` | 计划固定 1 轮，以及实际完成/成功轮数 |
| `tokens` | 总 tokens、每轮 tokens、来源（state 或 rounds） |
| `cost` | 按 tokens × 单价换算的估算美元；没设单价时为 `null`，不伪造金额 |
| `state` | state 路径、phase、轮数、tokensTotal、workspace |
| `log` / `logExists` | soak 日志路径 / 是否存在 |
| `budget` | 本次实际使用的单轮预算与最坏合计 |
| `skipReasons` | SKIP 的具体原因；非 SKIP 为 `null` |
| `paidReport` | 底层 `paid-report.json` 路径，便于继续查明细 |

其他产物仍由底层脚本产出，排查时以它们为准：

```text
${TMPDIR:-/tmp}/ui-paid-smoke.log
${TMPDIR:-/tmp}/ui-paid-smoke-state.json
${TMPDIR:-/tmp}/ui-paid-smoke-state.json.rounds.jsonl
${TMPDIR:-/tmp}/ui-paid-smoke-ws/.ui/journal.jsonl
${TMPDIR:-/tmp}/ui-paid-smoke-paid-report.json
```

这些路径都可用对应的 `UI_SMOKE_LOG` / `UI_SMOKE_STATE` /
`UI_SMOKE_ROUNDS_LOG` / `UI_SMOKE_WS` / `UI_SMOKE_PAID_REPORT` 覆盖。
`smoke-report.json` 是摘要，不是这些材料的替代品。

## 运行示例

真实 provider（默认适配器）：

```bash
UI_PAID_API_KEY="sk-..." \
UI_PAID_MODEL="deepseek-flash@2026-11" \
UI_SOAK_ALLOW_COST=1 UI_SOAK_YES=1 \
bash tools/soak_paid_smoke.sh
```

固定脚本化适配器（任何能读 stdin、写 stdout 的进程）：

```bash
UI_PAID_API_KEY="sk-..." \
UI_PAID_MODEL="my-provider@2026-11" \
UI_PAID_MODEL_CMD="python3 /path/to/adapter.py" \
UI_SOAK_ALLOW_COST=1 UI_SOAK_YES=1 \
bash tools/soak_paid_smoke.sh
```

`UI_PAID_MODEL_CMD` 的命令内容不会回显或写进 report；不要把 key 写进命令
（进程列表可见），key 只放环境变量。

## key 管理

- key 只通过环境变量 `UI_PAID_API_KEY` 或 `OPENAI_API_KEY` 传入；
- 不写进仓库、不写进 `UI_PAID_MODEL_CMD`、不写进 state/report；
- 报告只有 `keySource` 之类的来源说明（在底层 `paid-report.json` 里），没有值；
- 不要 `export` 到长期 shell profile；跑完就关闭终端或重新登录；
- 在 provider 侧设置预算/告警，不能只依赖本地 `max_usd`；
- 怀疑泄露时先轮换 key，再排查适配器和 shell 历史。

收尾自检：

```bash
for f in /tmp/ui-paid-smoke-report.json /tmp/ui-paid-smoke.log \
         /tmp/ui-paid-smoke-state.json /tmp/ui-paid-smoke-paid-report.json; do
  [ -f "$f" ] && grep -qF -- "$UI_PAID_API_KEY" "$f" && echo "LEAK $f"
done
```

底层 `tools/soak_paid.sh` 会在收尾时扫描 log/state/rounds，把完整 key 替换成
`[REDACTED]`；如果连 smoke-report 都被污染，本脚本会把 report 换成最小失败
报告并 `exit 3`。这属于适配器 bug，应修适配器，而不是依赖兜底。

## 失败排查

先看 `smoke-report.json` 的 `phase` / `exitCode` / `error` / `skipReasons`：

1. **SKIP（exit 0）**：没有产生调用。看 `skipReasons`，补齐四道闸；
   不要为了「让它跑」而把 key 写进脚本或 CI。
2. **配置错误（exit 2）**：预算非法、缺少 `node`（默认适配器）、底层配置错误。
   同样没有调用。先修配置再重跑。
3. **运行失败（exit 1）**：看 `state.lastError`，再按顺序看：
   - `log` 里 `=== round 1 ui run ===` / `=== round 1 ui verify ===` 后面的原始输出；
   - `state.rounds.jsonl` 的 `run` / `verify` / `writeCalls` / `shellCalls`；
   - `${WS}/.ui/journal.jsonl` 里真实的工具事件；
   - `ui verify --workspace "$UI_SMOKE_WS"` 复查哈希链。
4. **中断（exit 130）**：底层会先杀掉当前 `ui` 进程再写 failed state；
   不要假设「没写 report 就没花钱」，以 provider 账单为准。
5. **exit 3**：smoke-report 里检测到 key，已替换成最小失败报告。先修适配器。

常见原因：

| 现象 | 可能原因 | 处理 |
|---|---|---|
| 立刻 SKIP | 四道闸缺一 | 逐项检查环境变量；不要跳过闸门 |
| exit 2，`max_usd` 报错 | 预算不是大于 0 的数字 | 用普通十进制数字，如 `0.01` |
| exit 2，`node` 报错 | 默认适配器需要 node | 装 node，或设置 `UI_PAID_MODEL_CMD` |
| 401/403 | key 错误/过期/无权访问该模型 | 修 key 或模型名，不要自动重试 |
| 429/5xx | provider 限流或抖动 | 看适配器日志；本脚本不自动重试 |
| 超时 | 单轮 300 秒不够，或模型卡住 | 先看日志尾部；确有必要再显式提高 `UI_SMOKE_RUN_TIMEOUT` |
| `state.phase=running` 但进程已退出 | 底层没走到收尾 | 以日志尾部、rounds、journal 为准，不要当成「还在跑」 |

脚本不自动重试。修好 provider/网络/任务后**手动**重跑；重跑前先复制
`smoke-report.json`、state、日志和 `.ui/journal.jsonl`，因为 `soak_real.sh`
默认会清空 workspace。

## 没有真实 provider 时怎么办

正确做法是让它 SKIP，并明确记录「没有跑真实 provider」：

```bash
bash tools/soak_paid_smoke.sh
# exit 0, phase=skipped, skipReasons 列出缺的闸
```

不要用 `--mock`、假 key 或改脚本的方式把 SKIP 变成 `passed`；那会伪造调用证据。

如果只想验证包装器/链路本身（不联网、不证明真实 provider），可以用临时
scripted provider。把 [docs/50-real-soak.md](50-real-soak.md) 里的
`/tmp/scripted_provider.py` 存好，然后：

```bash
UI_PAID_API_KEY="test-key-not-real-1234" \
UI_PAID_MODEL="scripted-paid-smoke" \
UI_PAID_MODEL_CMD="python3 /tmp/scripted_provider.py" \
SOAK_PROVIDER_STATE=/tmp/paid-smoke-provider-state.json \
UI_SOAK_ALLOW_COST=1 UI_SOAK_YES=1 \
UI_SMOKE_USD_PER_MTOK=1.0 \
UI_SMOKE_REPORT=/tmp/paid-smoke-selftest-report.json \
UI_SMOKE_LOG=/tmp/paid-smoke-selftest.log \
UI_SMOKE_STATE=/tmp/paid-smoke-selftest-state.json \
UI_SMOKE_WS=/tmp/paid-smoke-selftest-ws \
UI_SMOKE_PAID_REPORT=/tmp/paid-smoke-selftest-paid.json \
bash tools/soak_paid_smoke.sh
```

预期：

- `rounds.planned=1`、`rounds.completed=1`，日志里只有一轮 `=== round 1 ui run ===`；
- `phase=passed`、`tokens.total > 0`、设置单价时 `cost.usd > 0`；
- `state.phase=passed`、`rounds=1`；
- report/state/log 里都没有临时 key；
- 退出码 0。

这**只证明包装器、预算透传、state/report 汇总能工作**，不证明任何真实
provider 可用，也不证明真实计费金额。本次开发环境只有 WSL + `python3`，没有
`node` 和付费端点，因此只跑了 SKIP、缺成本闸/确认、非法预算和 scripted 端到端；
**真实付费 provider 没有实跑**。

## 边界

- 1 轮 smoke 只说明「能跑完」，不说明长时间稳定；24h/多轮稳定性用
  [docs/48-soak.md](48-soak.md) 的 mock soak 和 [docs/50-real-soak.md](50-real-soak.md)
  的真实长跑，付费长跑用 [docs/53-paid-soak.md](53-paid-soak.md)。
- 不并发、不压测、不主动制造限流/断网；重试属于适配器行为。
- token 可能来自 provider usage 或内核估算；金额是估算，账单以 provider 为准。
- 不替代人类判断：1 轮通过不等于可以上线，失败也不等于模型不可用（先看日志）。
