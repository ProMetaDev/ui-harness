# 真实 provider 长跑（real soak）

> 脚本：[tools/soak_real.sh](tools/soak_real.sh)
> 对照：[docs/48-soak.md](48-soak.md)（mock 24h soak）

## 它证明什么、不证明什么

`soak_real.sh` 每轮用**真实 provider** 跑一次 `ui run`，再跑一次 `ui verify`，
检查模型是否真的调用了工具、文件是否真的落盘、journal 链是否仍然完整。

它和 24h 本地 soak 是两条互补的线，**不能互相替代**：

| | mock 24h soak（`tools/soak_long.sh`） | 真实 provider 长跑（`tools/soak_real.sh`） |
|---|---|---|
| 模型 | `--mock` 剧本，零成本、确定性 | 真实 provider，按 token 计费、有网络/限流波动 |
| 证明 | 进程、RSS/fd、journal 增长、长时间稳定性 | 模型真的会调工具、日志能验、失败可见 |
| 不证明 | 模型可用性 | 24h 稳定性；几轮通过不代表一天不坏 |
| 默认 | 本地手动跑 24h | **默认 SKIP**，显式开成本闸才跑 |

所以：真实 provider 长跑不替代 24h 本地 soak；反过来，mock 全绿也不能当作
“真实模型已经跑通”。

## 成本与跳过条件

默认不跑、不产生任何模型调用：

```bash
bash tools/soak_real.sh
# SKIP soak-real：真实 provider 长跑未启用（没有产生任何模型调用）
```

只有下面两个条件**同时**满足才真正执行：

1. `UI_MODEL_CMD` 非空（指向一个真实的模型进程）；
2. `UI_SOAK_ALLOW_COST=1`（显式打开成本闸）。

脚本不会自动重试。任何一轮的 `ui run` 或 `ui verify` 失败都会立即停止，
并把 `phase=failed` 和原因写进 state 文件——重试会让“偶发失败”看起来像
“最终成功”，也会让成本翻倍。

开跑前脚本会打印：

- 轮数、间隔、单轮超时；
- **预计模型调用次数**：下限 `2 × 轮数`（一次工具调用 + 一次收尾）；
  默认任务常见 `3 × 轮数`（写、读、收尾）；模型走弯路时更多；
- 最坏墙钟（由 `UI_SOAK_RUN_TIMEOUT` 封顶时）和预算参数；
- 明确提示真实 provider 按 token 计费、脚本本身不做金额兜底（除非显式传
  `UI_SOAK_MAX_USD`）。

`ui verify` 只读本地 journal，**不调用模型、不产生费用**。

## 前置条件

- **固定 commit**：用当前 checkout 的 HEAD 跑。脚本会把 `commit` 写进日志和
  state；结论要能对应到一个确切的提交。
- **固定模型**：`UI_MODEL_LABEL` 写人可读的模型标签（例如
  `deepseek-flash@2026-11`），不要写密钥。真实端点/模型仍由 `UI_MODEL_CMD`
  和它的配置决定。
- **固定预算**：显式设置 `UI_SOAK_MAX_TOKENS` / `UI_SOAK_MAX_CALLS` 等，
  或明确接受 `ui` 的默认上限。不同预算下的“通过”不可比。
- **密钥不入库**：真实 key 放在环境变量或仓库外的配置文件里
  （`chat_compat.mjs` 默认读 `secrets/astra.json`，该目录已被忽略）。
- 已构建 `ui` 二进制（`cargo build --release`，或让 `tools/env.sh` 找到
  `UI_BIN`）。

## 运行

默认 SKIP（不花钱）：

```bash
bash tools/soak_real.sh
```

真实 provider（示例：OpenAI 兼容端点）：

```bash
UI_SOAK_ALLOW_COST=1 \
UI_MODEL_CMD="node tools/chat_compat.mjs" \
UI_SOAK_MODEL_LABEL="deepseek-flash@2026-11" \
UI_SOAK_ROUNDS=3 \
UI_SOAK_INTERVAL=10 \
UI_SOAK_MAX_TOKENS=200000 \
UI_SOAK_MAX_CALLS=60 \
UI_SOAK_MAX_USD=0.50 \
UI_SOAK_WS=/tmp/ui-soak-real \
UI_SOAK_LOG=/tmp/ui-soak-real.log \
UI_SOAK_STATE=/tmp/ui-soak-real-state.json \
bash tools/soak_real.sh
```

`UI_MODEL_CMD` 不绑定某个 provider API：进程从 stdin 读一行快照 JSON，
向 stdout 写一行 `{"content": "...", "toolCalls": [...]}` 即可。
`tools/chat_compat.mjs`、`tools/deepseek.mjs` 都是这种适配器。

## 默认任务与每轮判定

默认任务（`UI_SOAK_TASK` 未设置时）：

> 创建或修改当前工作区的 `soak.txt`：写入一行以 `soak round` 开头的确认内容
> （轮次用本任务末尾括号里的数字），然后用 `fs_read` 读回 `soak.txt`，确认
> 内容正确后说明结果。必须至少执行一次 `fs_write` 或 `shell` 真的落盘。

脚本会在每轮任务末尾追加“（第 N 轮）”，因此同一个 workspace 连续跑也能
分辨轮次。每轮判定四件事：

1. `ui run` 退出码为 0，且输出里的结束原因是 `done`；
2. `ui verify` 退出码为 0，且输出包含“哈希链完整”；
3. **本轮新增** journal 里至少有一次成功的 `fs_write` / `fs_edit` / `fs_mv`
   或 `shell`（`tool/end` 且 `ok=true`）——只看工具调用，不看模型自述；
4. 使用默认任务时，轮末 `soak.txt` 存在且非空。

第 3 条是 provider API 无关的：它读的是 journal 里的真实工具事件，不解析
模型返回的文本。自定义任务如果只读不写，可以设 `UI_SOAK_REQUIRE_TOOL=0`
关掉判定（仍会记录工具次数，但不因“没有写操作”失败）。

每轮记录：耗时（run / verify / 总计）、退出码、journal 事件数（总量与本轮
增量）、成功写入/`shell`/读取次数、`约 tokens`、workspace 字节数。

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `UI_SOAK_ALLOW_COST` | 未设置 | 必须为 `1`，否则 SKIP |
| `UI_MODEL_CMD` | 未设置 | 真实模型进程命令；为空则 SKIP |
| `UI_SOAK_ROUNDS` | `3` | 轮数，至少 1 |
| `UI_SOAK_INTERVAL` | `10` | 轮间隔秒（非负整数） |
| `UI_SOAK_WS` | `/tmp/ui-soak-real` | 工作区；**默认开跑前清空** |
| `UI_SOAK_LOG` | `/tmp/ui-soak-real.log` | 追加式日志（含 ui 原始输出） |
| `UI_SOAK_STATE` | `/tmp/ui-soak-real-state.json` | 状态文件（原子替换） |
| `UI_SOAK_TASK` | 见上 | 自定义任务文本 |
| `UI_SOAK_MODEL_LABEL` | `UI_MODEL` 或 `unspecified` | 写进日志/state 的模型标签 |
| `UI_SOAK_RUN_TIMEOUT` | `1800` | 单轮 `ui run` 超时秒；`0` 关闭 |
| `UI_SOAK_MAX_TOKENS` | 未设置 | 传给 `ui run --max-tokens` |
| `UI_SOAK_MAX_CALLS` | 未设置 | 传给 `ui run --max-calls` |
| `UI_SOAK_MAX_STEPS` | 未设置 | 传给 `ui run --max-steps` |
| `UI_SOAK_MAX_USD` | 未设置 | 传给 `ui run --max-usd` |
| `UI_SOAK_USD_PER_MTOK` | 未设置 | 传给 `ui run --usd-per-mtok` |
| `UI_SOAK_REQUIRE_TOOL` | `1` | `1` 要求工具证据；`0` 只记录 |
| `UI_SOAK_KEEP_WS` | `0` | `1` 保留已有 workspace（不清空） |

`UI_SOAK_WS` 默认会被清空；脚本拒绝把它指向 `/`、`$HOME` 或仓库根。
需要保留上一轮产物时用 `UI_SOAK_KEEP_WS=1`。

## 状态、日志与失败排查

state 文件是原子写的 JSON，phase 取值：

- `running`：正在跑（`updatedAt` 每轮刷新）；
- `passed`：所有轮都通过；
- `failed`：某轮失败或被信号中断；中断时会先杀掉当前 `ui` 进程再写 state，
  避免把一个还在计费的进程留在后台。
- `skipped`：成本闸/ provider 未满足，**没有跑**。

每轮的明细同时追加到 `$UI_SOAK_STATE.rounds.jsonl`（可用
`UI_SOAK_ROUNDS_LOG` 覆盖），最后一轮也内嵌在 state 的 `lastRound` 里。

失败时按顺序看：

1. `lastError`：`round=N stage=run|verify|evidence` 和具体原因；
2. 日志里的轮次标记：`=== round N ui run ===` / `=== round N ui verify ===`，
   后面紧跟该轮 `ui run` / `ui verify` 的原始输出；
3. `$UI_SOAK_WS/.ui/journal.jsonl`：本轮真实工具事件；
4. `ui verify --workspace "$UI_SOAK_WS"` 复查链；
5. 需要回放轨迹时用 `ui replay`，或 `ui export --html` 导出单文件查看器。

脚本不会自动重试。修好 provider/网络/任务后**手动**重跑；如果需要保留失败
现场，先复制 `$UI_SOAK_LOG`、state 和 `.ui/journal.jsonl` 再重跑（默认会清空
workspace）。

## 离线自测（不花钱）

没有真实 provider 时，可以用一个临时 scripted provider 验证整条链路
（`ui run` → 工具执行 → `ui verify` → 判定/state）。把下面内容存成
`/tmp/scripted_provider.py`：

```python
import json, os, re, sys
prompt = json.load(sys.stdin).get("prompt", "")
round_no = (re.search(r"第\s*(\d+)\s*轮", prompt) or [None, "1"])[1]
state_path = os.environ.get("SOAK_PROVIDER_STATE", "/tmp/soak-provider-state.json")
state = {}
if os.path.exists(state_path):
    state = json.load(open(state_path, encoding="utf-8"))
if state.get("round") != round_no:
    state = {"round": round_no, "wrote": False, "read": False}
if not state["wrote"]:
    state["wrote"] = True
    reply = {"content": "写入", "toolCalls": [
        {"name": "fs_write", "args": {"path": "soak.txt", "content": f"soak round {round_no}\n"}}]}
elif not state["read"]:
    state["read"] = True
    reply = {"content": "读回", "toolCalls": [
        {"name": "fs_read", "args": {"path": "soak.txt"}}]}
else:
    reply = {"content": f"第 {round_no} 轮完成", "toolCalls": []}
json.dump(state, open(state_path, "w", encoding="utf-8"))
print(json.dumps(reply, ensure_ascii=False))
```

然后：

```bash
UI_SOAK_ALLOW_COST=1 \
UI_MODEL_CMD="python3 /tmp/scripted_provider.py" \
SOAK_PROVIDER_STATE=/tmp/soak-provider-state.json \
UI_SOAK_ROUNDS=2 UI_SOAK_INTERVAL=1 \
UI_SOAK_WS=/tmp/ui-soak-real-selftest \
UI_SOAK_LOG=/tmp/ui-soak-real-selftest.log \
UI_SOAK_STATE=/tmp/ui-soak-real-selftest-state.json \
bash tools/soak_real.sh
```

预期看到每轮 `status=ok`、`write:1`、`read:1`，最后 `PASS soak-real`。
这个自测只证明脚本链路和判定逻辑通，**不证明任何真实 provider 可用**。

## 边界

- 默认 3 轮是冒烟/回归，不是 24h soak；长时间稳定性仍以
  [docs/48-soak.md](48-soak.md) 的 mock soak 为主。
- 不主动制造 provider 故障、限流、网络抖动；重试策略属于适配器自身行为。
- 不测 RSS/fd 等资源指标；那是 `soak_long.sh` 的职责。
- 单机、单 workspace、串行执行；不并发压测。
- token 数来自 provider 返回的 usage，可能为近似值；账单以 provider 为准。
- 真实模型有随机性：失败是信号，不应靠“再跑一次”掩盖；脚本本身不重试。
