# 真实付费 provider 长跑（paid soak）

> 脚本：[tools/soak_paid.sh](tools/soak_paid.sh)
> 底层：[tools/soak_real.sh](tools/soak_real.sh) · 说明：[docs/50-real-soak.md](50-real-soak.md)
> 默认适配器：[tools/chat_compat.mjs](tools/chat_compat.mjs)

`tools/soak_paid.sh` 是 `tools/soak_real.sh` 的**付费 provider 包装器**：它不复制
轮次、工具判定、`ui verify` 或 state 逻辑，只准备「付费运行」需要的凭据、模型、
预算和人工确认，然后调用 `soak_real.sh`；跑完把 commit/model/tokens/耗时/state
汇总成 `paid-report.json`。

它回答的问题是：**在明确知道模型、端点、预算和代价的前提下，让真实 provider
跑几轮带工具的任务，并把这次运行固定下来。** 它不回答「24 小时稳不稳定」，也
不替代 [docs/48-soak.md](48-soak.md) 的 mock 长跑。

## 为什么默认 SKIP

这个入口一旦误跑，代价不是多几行日志，而是**真实账单**。最常见的三种误跑是：

1. CI 或 shell profile 里恰好有 `OPENAI_API_KEY`，某个脚本顺手调用了它；
2. 想跑测试，却把 key 和 `UI_SOAK_ALLOW_COST` 一起带进了环境；
3. 没注意模型/端点已经换成了更贵的一档。

所以脚本不猜、不兜底、不自动重试：

- 凭据、模型、成本开关、人工确认**四道闸全部满足**才真正启动；
- 缺任意一道：打印 `SKIP`，**exit 0，不产生任何模型调用**；
- 预算/轮数等配置值非法：`exit 2`，同样不产生调用；
- 没有交互终端又没设 `UI_SOAK_YES=1`：按 SKIP 处理，不假设「没人看着也可以花」。

直接运行：

```bash
bash tools/soak_paid.sh
# SKIP paid-soak：未进入真实计费运行（没有产生任何模型调用）
```

这次 SKIP 也会写一份 `paid-report.json`（`phase=skipped`、`tokens.total=0`、
`skipReasons` 列出缺的闸），让「没跑」和「跑失败」在文件层面分得清。

## 四道闸

| 闸 | 变量 | 缺失/不满足时 |
|---|---|---|
| 凭据 | `UI_PAID_API_KEY` 或 `OPENAI_API_KEY` | SKIP；不读 `UI_CHAT_KEY` 当闸门，避免用错 key |
| 模型 | `UI_PAID_MODEL` | SKIP；不猜默认模型，避免跑错价格档 |
| 成本开关 | `UI_SOAK_ALLOW_COST=1` | SKIP；该值必须显式为 `1` |
| 人工确认 | 交互输入 `yes`，或 `UI_SOAK_YES=1` | 非交互且未设 `UI_SOAK_YES` 时 SKIP |

`UI_SOAK_YES=1` 只用于非交互场景（CI 自测、受控脚本）。它**不是**成本开关，
仍然需要 `UI_SOAK_ALLOW_COST=1`。

## 运行前会打印什么

四道闸通过后、真正调用前，脚本会打印：

- **模型**：`UI_PAID_MODEL`（同时写入 state 的 `modelLabel` 和 report 的 `model`）；
- **base URL（脱敏）**：去掉 userinfo / query / fragment，只留
  `scheme://host[:port]/path`；如果 key 被误写进 path，替换成 `[REDACTED]`；
- **预计模型调用下限**：`2 × 轮数`（一次工具调用 + 一次收尾）；默认任务常见
  `3 × 轮数`（写、读、收尾）；模型走弯路或适配器重试时更多；
- **预算上限**：每轮 `max_tokens` / `max_calls` / `max_usd`，以及最坏合计；
- **USD 是否硬中止**（见下一节）；
- **预计墙钟**：由单轮超时和轮间隔封顶；
- **计费风险**：真实端点按 token 计费；触顶只中止后续调用，已经发生的调用仍会
  计费；适配器内部可能重试；token 统计不是账单。

打印完之后才要求确认。确认前不会启动 `soak_real.sh`，也不会启动模型进程。

## 适配器与环境映射

默认命令是：

```bash
UI_MODEL_CMD="node tools/chat_compat.mjs"
```

`tools/chat_compat.mjs` 读取的环境变量是 `UI_CHAT_BASE` / `UI_CHAT_KEY` /
`UI_MODEL`（以及 `UI_CHAT_PATH`、`UI_CHAT_STREAM`、`UI_CHAT_TIMEOUT_MS`、
`UI_CHAT_RETRIES`）。包装器把付费入口的变量映射过去：

| 包装器输入 | 传给适配器 |
|---|---|
| `UI_PAID_API_KEY` 或 `OPENAI_API_KEY` | `UI_CHAT_KEY`（后者覆盖环境里已有的值） |
| `UI_PAID_BASE_URL` / `OPENAI_BASE_URL` / `UI_CHAT_BASE` | `UI_CHAT_BASE`（按此优先级） |
| `UI_PAID_MODEL` | `UI_MODEL`，同时作为 `UI_SOAK_MODEL_LABEL` |

没有设置 base URL 时，`chat_compat.mjs` 仍可回落到 `secrets/astra.json`（该目录
被 `.gitignore` 忽略）；但 report 里只会记录脱敏后的 base，未设置时显示
「由适配器配置决定」。

需要非 OpenAI 兼容端点或本地假 provider 时，用 `UI_PAID_MODEL_CMD` 覆盖：

```bash
UI_PAID_MODEL_CMD="python3 /tmp/scripted_provider.py" \
UI_PAID_API_KEY="sk-..." \
UI_PAID_MODEL="scripted-paid-e2e" \
UI_SOAK_ALLOW_COST=1 UI_SOAK_YES=1 \
bash tools/soak_paid.sh
```

约定：

- `UI_PAID_MODEL_CMD` 的**命令内容不会回显到终端或写进 report**，避免命令里
  夹带凭据；请不要把 key 写进命令（进程列表会看到），key 只放环境变量。
- 环境里已有的 `UI_MODEL_CMD` 会被**忽略**（会打印一条 WARN）；付费入口只认
  默认适配器或 `UI_PAID_MODEL_CMD`，避免普通 mock 命令被误当成付费 provider。
- 默认适配器需要 `node` 在 `PATH` 里；没有 `node` 时脚本在确认前就 `exit 2`，
  不会调用模型。

## 预算与中止

默认值（都可用环境变量覆盖）：

| 变量 | 默认 | 说明 |
|---|---|---|
| `UI_SOAK_ROUNDS` | `2` | 轮数；比 `soak_real.sh` 的 3 更保守 |
| `UI_SOAK_INTERVAL` | `10` | 轮间隔秒 |
| `UI_PAID_RUN_TIMEOUT` / `UI_SOAK_RUN_TIMEOUT` | `900` | 单轮 `ui run` 超时秒，`0` 关闭 |
| `UI_SOAK_MAX_TOKENS` | `200000` | 每轮 token 上限 |
| `UI_SOAK_MAX_CALLS` | `60` | 每轮工具调用上限 |
| `UI_SOAK_MAX_USD` | `0.50` | 每轮金额上限；是否硬生效见下 |
| `UI_PAID_USD_PER_MTOK` | 未设置 | 每百万 token 美元价；设置后 `--max-usd` 才硬生效 |
| `UI_SOAK_REQUIRE_TOOL` | 强制 `1` | 每轮必须有成功的写/改/移动或 `shell` 证据 |

**预算按「每轮一次 `ui run`」重置。** `soak_real.sh` 每轮调用一次 `ui run`，
所以上面的 token/call/USD 是单轮上限；最坏合计是 `轮数 × 单轮上限`。脚本会在
report 的 `budget.worstCaseTotal` 里给出合计值。

金额上限有一个必须说清楚的细节：

- `ui` 的 `--max-usd` 只有在同时给出 `--usd-per-mtok` 时才会计费并中止；内核
  **不猜**模型价格（`usd_per_mtok=0` 表示不按金额计费）。
- 包装器不会为了「看起来有金额上限」而编一个价格。只有设置了
  `UI_PAID_USD_PER_MTOK`（> 0）才启用 USD 硬中止；否则 `--max-usd` 只是名义值，
  真正硬中止的是 `max_tokens` / `max_calls` / 墙钟。
- report 的 `budget.perRound.usdHardCap` 会明确写 `true` / `false`；为 `false`
  时不要把它当成金额兜底。

其他中止规则：

- 任何一轮 `ui run` 或 `ui verify` 失败，立即停止，不自动重试；
- 单轮超时按失败退出，不重试；
- 包装器被 `INT`/`TERM` 时转发给 `soak_real.sh`；`soak_real.sh` 会先杀掉当前
  `ui` 进程再写 `phase=failed`，避免把还在计费的进程留在后台；
- `UI_SOAK_REQUIRE_TOOL=1` 是强制的：本轮 journal 里没有成功的
  `fs_write` / `fs_edit` / `fs_mv` / `shell` 就判失败。自定义只读任务不适合这个
  入口；请直接用 `soak_real.sh`。

## 产物：paid-report.json

默认路径 `UI_PAID_REPORT=/tmp/ui-paid-soak-report.json`，原子写入，权限 `0600`。
主要字段：

| 字段 | 含义 |
|---|---|
| `phase` | `passed` / `failed` / `skipped`，以 `soak_real.sh` 的 state 为准 |
| `exitCode` | 包装器最终退出码（0 / 1 / 2 / 3 / 130） |
| `commit` | `soak_real.sh` 记录的启动时 HEAD；state 缺失时为 `null` |
| `model` | `UI_PAID_MODEL`（或 state 里的 `modelLabel`） |
| `baseUrlRedacted` | 脱敏后的 base URL |
| `provider` | 默认适配器或「custom UI_PAID_MODEL_CMD」；不回显命令 |
| `keySource` | 只记来源变量名（`UI_PAID_API_KEY` / `OPENAI_API_KEY`），不记值 |
| `rounds` | 计划/完成/成功轮数，以及每轮 ok、tokens、耗时、写/shell 次数 |
| `budget` | 单轮上限、最坏合计、`usdHardCap` |
| `tokens` | `state.tokensTotal` 或 rounds 汇总；附来源与「不是账单」说明 |
| `timing` | 墙钟、state 墙钟、每轮耗时、run/verify 合计 |
| `state` | state 路径、phase、runId、lastError、lastRound 等 |
| `skipReasons` | SKIP 的具体原因；非 SKIP 为 `null` |
| `sanitized` | 是否在 log/state 里检测到 key 并替换成 `[REDACTED]` |
| `notes` | 解释性说明（含「不是 provider 账单或收据」） |

`state`、`log`、`rounds.jsonl` 仍然由 `soak_real.sh` 产出：

```text
/tmp/ui-paid-soak-state.json
/tmp/ui-paid-soak-state.json.rounds.jsonl
/tmp/ui-paid-soak.log
/tmp/ui-paid-soak/.ui/journal.jsonl
```

report 是这些材料的**摘要**，不替代它们；排查失败时要看 state 和日志原文。

## key 不进 log/state/report

- key 只通过环境变量传给适配器，不进入 `UI_MODEL_CMD`、state、report 或
  `soak_real.sh` 的日志；
- report 只有 `keySource`（变量名），没有值；
- 如果自定义适配器把 key 打到了 stderr，`soak_real.sh` 会把它写进日志。包装器
  在收尾时扫描 log/state/rounds，发现完整 key 就替换成 `[REDACTED]`，并在 report
  标记 `sanitized=true`；如果连 report 都被污染，report 会被替换成最小失败报告，
  退出码 `3`。这属于适配器 bug，应修适配器，而不是依赖兜底。

自检：

```bash
for f in /tmp/ui-paid-soak-report.json /tmp/ui-paid-soak-state.json /tmp/ui-paid-soak.log; do
  if grep -qF -- "$UI_PAID_API_KEY" "$f"; then echo "LEAK $f"; fi
done
```

## 固定 commit 与模型

结论只在「同一个 commit + 同一个模型 + 同一组预算」下可比：

- `soak_real.sh` 启动时读取 HEAD，写进 `state.commit`；包装器把它抄进 report；
- 模型名来自 `UI_PAID_MODEL`，不要用「latest」这类会漂的标签；建议写成
  `deepseek-flash@2026-11` 这种可复查的形式；
- 预算写进 report；换预算后不要把两次 `passed` 直接对比；
- 适配器命令本身不写进 report（可能有凭据），因此复跑时还要固定
  `UI_PAID_MODEL_CMD` 和端点配置。

## 失败排查

先看 report 的 `phase` / `exitCode` / `errorKind`：

1. **SKIP（exit 0）**：看 `skipReasons`。常见是缺 key/model、没设
   `UI_SOAK_ALLOW_COST=1`、非交互又没设 `UI_SOAK_YES=1`。没有模型调用。
2. **配置错误（exit 2）**：`errorKind=config`。例如轮数不是正整数、预算不是数字、
   默认适配器找不到 `node`。同样没有模型调用。如果退出码是 2 但
   `errorKind=run`，通常不是包装器的配置错误，而是 `soak_real.sh` 自己提前退出；
   按下面的 `state.phase=running` 处理。
3. **运行失败（exit 1）**：看 `state.lastError`（`round=N stage=run|verify|evidence`
   加原因），再按顺序看：
   - 日志里的 `=== round N ui run ===` / `=== round N ui verify ===` 后面的原始输出；
   - `$UI_SOAK_WS/.ui/journal.jsonl` 里本轮的真实工具事件；
   - `ui verify --workspace "$UI_SOAK_WS"` 复查哈希链；
   - 需要回放时用 `ui replay`，或 `ui export --html` 导出单文件查看器。
4. **被中断（exit 130）**：`errorKind=interrupted`；state 已写 `phase=failed`，
   当前 `ui` 进程已被 `soak_real.sh` 杀掉。不要假设「没写 report 就没花钱」，
   以 provider 账单为准。
5. **`sanitized=true` 或 exit 3**：key 出现在了日志/state/report。先修适配器，
   不要继续用它跑付费任务。

如果 `state.phase=running` 或 state 缺失但退出码非 0，说明 `soak_real.sh` 在写
`failed` state 之前就退出了（例如它自身的输出/环境问题）。包装器仍会按
`failed` 记录 report；此时以日志尾部、`rounds.jsonl` 和 journal 为准排查，不要
把 `running` 当成「还在跑」或「没失败」。

脚本不自动重试。修好 provider/网络/任务后**手动**重跑；重跑前先复制
`paid-report.json`、state、日志和 `.ui/journal.jsonl`，因为 `soak_real.sh`
默认会清空 workspace。

## 离线自测（不花钱，不证明真实 provider）

没有真实端点时，可以用一个临时 scripted provider 验证整条链路。这个脚本直接
复用 [docs/50-real-soak.md](50-real-soak.md) 里的 `/tmp/scripted_provider.py`
（它不联网、不读 key），只是换成付费入口：

```bash
UI_PAID_API_KEY="test-key-not-real" \
UI_PAID_MODEL="scripted-paid-e2e" \
UI_PAID_MODEL_CMD="python3 /tmp/scripted_provider.py" \
SOAK_PROVIDER_STATE=/tmp/paid-provider-state.json \
UI_SOAK_ALLOW_COST=1 UI_SOAK_YES=1 \
UI_SOAK_ROUNDS=2 UI_SOAK_INTERVAL=0 \
UI_SOAK_WS=/tmp/ui-paid-soak-selftest \
UI_SOAK_LOG=/tmp/ui-paid-soak-selftest.log \
UI_SOAK_STATE=/tmp/ui-paid-soak-selftest-state.json \
UI_PAID_REPORT=/tmp/ui-paid-soak-selftest-report.json \
bash tools/soak_paid.sh
```

预期：

- 每轮 `status=ok`，`write:1`、`read:1`；
- `state.phase=passed`，`rounds=2`；
- report `phase=passed`、`tokens.total > 0`、`commit` 是当前 HEAD；
- log/state/report 都不含临时 key。

这**只证明包装器、soak_real 链路、预算参数透传和 report 汇总能工作**，不证明
任何真实 provider 可用，也不证明真实计费金额。

本次改动所在的开发环境只有 WSL + `python3`，没有 `node`，也没有付费端点，
因此只跑了 SKIP、缺失成本闸、未确认、非法配置和上面的 scripted 端到端；
**真实付费 provider 没有实跑**。真实结论必须在有 key、有端点、有 `node` 的
机器上按本文档跑出来后记录。

## 边界

- 只做冒烟/回归（默认 2 轮），不是 24h soak；长时间稳定性用
  [docs/48-soak.md](48-soak.md) 和 [tools/soak_long.sh](tools/soak_long.sh)。
- 只做单机、单 workspace、串行执行；不并发压测。
- 不主动制造 provider 故障/限流/网络抖动；重试策略属于适配器自身行为。
- token 来自 provider 返回的 usage 或 `ui` 的估算，可能不精确；账单以 provider
  为准。
- 真实模型有随机性：失败是信号，不应靠「再跑一次」掩盖；包装器本身不重试。
