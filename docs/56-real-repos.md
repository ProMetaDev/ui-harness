# 真实多仓库评测（框架）

> **状态：只有框架，没有真实结果。**
>
> 截至 2026-11-27，10 个真实仓库、12 个任务的正式评测**一次都没有跑**。
> 已交付的是：仓库/任务校验、`ui index` 指标、`ui run` 记录、`ui verify` 校验、
> expect 判定、失败隔离、`report.json` / `report.md`，以及用 12 个**合成 fixture**
> 做的 dry-run 自测。合成 fixture 不是真实仓库，不能产生可对外引用的数字。

相关文件：

- `tools/bench_repos.py`：评测执行器（Python 3.8+ 标准库）。
- `tools/make_bench_fixtures.sh`：生成 12 个合成多文件仓库（每个约 1500 行，明确非真实）。
- `tools/verify_bench_repos.sh`：自测 dry-run/index、占位符 SKIP、越界拒绝、报告结构。
- `bench/repos.example.json`：10 个真实仓库候选，`path` 全部是 `REPLACE_ME/...`。
- `bench/tasks.example.json`：12 个通用任务。

## 1. 为什么叫“框架”，不叫“评测结果”

评测结果至少要能回答：**哪个 commit、什么模型、什么任务、花了多少 token/时间、改了什么、怎么验证的**。
仓库里已有的 `bench_ml.sh` 是 3 个 CPU 任务的工具链闭环，`bench_real.sh` 是单仓库单任务对照；
它们都不是多仓库评测。为了避免把“框架跑通”说成“评测通过”，本目录的规则是：

1. 没有 provider 或显式 `--dry-run`：只做 `ui index`，任务状态写 `dry-run`，
   `success=0`，报告顶部写明“没有执行模型”。
2. `--mock`：`ui run` 真的执行，但模型是脚本化的；报告 `model.kind=scripted`，
   只验证管线，不代表模型能力。
3. 只有配置了真实 provider（`--provider-cmd` / `--provider`）且没有 `--dry-run`，
   报告才是 `mode=real`。
4. 单任务失败会被隔离记录，任务失败是评测数据；只有仓库校验/索引/任务异常才退出 2。

**不要把 dry-run 的 `success=0` 当成失败率，也不要把 mock 的数字当模型能力。**

## 2. 快速开始

先填仓库路径（把 `REPLACE_ME/...` 换成真实 clone 的绝对路径或相对于该 JSON 的路径）：

```bash
cp bench/repos.example.json bench/repos.json
# 编辑 bench/repos.json：10 个 path
```

先只做索引（不花钱，不调模型）：

```bash
python3 tools/bench_repos.py \
  --repos bench/repos.json \
  --tasks bench/tasks.example.json \
  --ui "$UI_BIN" \
  --out bench/out/dry-run
```

真跑一个仓库、一个任务（先控成本）：

```bash
python3 tools/bench_repos.py \
  --repos bench/repos.json \
  --tasks bench/tasks.example.json \
  --ui "$UI_BIN" \
  --only ripgrep \
  --task t01-repo-overview \
  --provider-cmd "node /abs/path/to/tools/deepseek.mjs" \
  --timeout 600 \
  --out bench/out/ripgrep-t01
```

`--provider-cmd` 只设置子进程的 `UI_MODEL_CMD`，命令原文不写进报告。
如果仓库内已有 `.ui/providers.json`，也可以用 `--provider NAME`。
`--mock FILE` 传 `ui run --mock`，用于无网络自测。

自测（不需要 provider，只跑合成 fixture）：

```bash
bash tools/verify_bench_repos.sh
```

没有 `ui` 二进制时该脚本直接 `SKIP`：索引必须由真实内核完成，不能造。

## 3. 选 10 个 >1 万行的真实仓库

### 3.1 选择标准

- **真实且有维护**：最近一年有提交，不是教学 toy；至少有 README、测试和构建文件。
- **多文件**：源文件数量足够让索引/检索有意义；不能是单文件脚本。
- **>1 万行**：以 clone 后 `ui index` 写出的 `indexedLines` 为准，不以 README/口碑为准。
  1 万行是下限，不是目标；选 1-10 万行之间的仓库更适合单机 600 秒超时。
- **可构建/可测试**：任务里的验证命令要能真跑；如果依赖私有服务，换仓库。
- **语言多样**：Rust/Python/Go/JS/TS/C 至少覆盖 4 种，避免指标只反映一种符号提取器。
- **许可与权限**：允许本地克隆和评测；不要把含密钥/客户数据的仓库放进清单。
- **固定 commit**：评测前记录 commit，最好 `git clone` 后 `git checkout <sha>`；
  工作区必须干净（报告会记录 `dirtyFiles`）。

`bench/repos.example.json` 里的 10 个名字只是**候选**（ripgrep、flask、requests、gin、tokio、
fastapi、express、jq、prometheus、vite），路径全是占位符，行数也必须在 clone 后用
`ui index` 实测。示例没有写死任何本机路径。

### 3.2 建议的目录布局

```text
/data/repos/ripgrep/        # git clone + checkout 固定 commit
/data/repos/flask/
...
bench/repos.json            # 本机路径清单，不入库
```

`bench/repos.example.json` 入库；真正的 `bench/repos.json` 建议放在本机或 CI secret 里，
因为它会暴露目录结构。报告里的 `path` 是仓库根，不包含 provider key。

### 3.3 任务集

`bench/tasks.example.json` 的 12 个任务是**通用冒烟任务**：概览、入口、构建/测试、
依赖、CI、最大文件、TODO、文档缺口、符号图、执行路径、小改动方案。它们能跨仓库复用，
但区分度有限。严肃评测应在此基础上增加仓库专属任务，例如：

```json
{
  "id": "flask-fix-404-handler",
  "prompt": "修复 tests/test_basic.py 里失败的 404 handler 用例；不要改测试。",
  "expect": {
    "changed": ["src/flask/app.py"],
    "answer_min_chars": 80
  },
  "verify": "python3 -m pytest tests/test_basic.py -q"
}
```

任务字段：

| 字段 | 必填 | 含义 |
|---|---|---|
| `id` | 是 | 唯一任务名；报告与 `--task` 都用它 |
| `prompt` | 是 | 原样传给 `ui run --task` |
| `expect` | 否 | 字符串或对象；见下表 |
| `verify` | 否 | 在仓库根执行的 shell 命令，rc=0 才算通过 |

`expect` 支持的键：

| 键 | 判定 |
|---|---|
| `answer_contains` | 字符串数组，answer 必须全部包含 |
| `answer_contains_any` | 字符串数组，至少命中一个 |
| `answer_not_contains` | 字符串数组，一个都不能出现 |
| `answer_min_chars` | answer 去掉首尾空白后的最小长度 |
| `changed` | 路径数组，必须都出现在本次实际变化文件里 |
| `changed_any` | 路径数组，至少一个出现在实际变化里 |
| `unchanged` | `true` 表示本次不能改任何文件 |
| `files` | 路径数组，运行后必须存在 |
| `contains` | `[{"path": "...", "text": "..."}]`，文件里必须包含文本 |
| `not_contains` | `[{"path": "...", "text": "..."}]`，文件里不能出现文本 |

`expect` 里的路径必须是仓库内相对路径，不能绝对路径、不能含 `..`。
未知键会判失败，避免拼写错误静默通过。
任务级 `verify` 在 `ui run` 之后执行，超时与 `--timeout` 相同；它的 rc 进入报告，
不通过时任务状态是 `failed`。

成本控制：完整 10 仓库 × 12 任务 = 120 次 `ui run`。先用 `--only` / `--task` 跑
1 仓库 × 1 任务，看报告里的 tokens 与耗时，再决定是否放大。框架不预估、不代填成本。

## 4. 每个仓库/任务实际执行什么

对每个 `status=ok` 的仓库：

1. `ui index --workspace <repo>`：记录 rc、files、symbols、reused/updated/removed、
   `indexedLines`（从 `.ui/index/manifest.json` 求和）、耗时。
2. 对每个任务：
   - 在 `.ui/bench/` 下生成本次专用 journal（不污染默认日志）；
   - 运行前后各做一次工作区 size+mtime 快照（排除 `.git/.ui/target/node_modules` 等），
     差集就是 `changedFiles`；
   - `ui run --workspace <repo> --task <prompt> --yes`：
     报告 rc、结束原因、tokens、toolCalls、steps、耗时、answer 长度与 sha256；
   - `ui verify --workspace <repo> --journal <本次日志>`：要求哈希链完整、没有未完成
     turn/tool；否则任务 `failed`；
   - 执行任务的 `verify` 命令（如果有）；
   - 按 `expect` 逐条判定，记录每条 check 的通过/失败与细节。

一个任务只有在 **run rc=0 且 reason=done、verify 干净、任务 verify rc=0、expect 不失败**
时才是 `success`。缺一不可。

## 5. 指标口径

| 指标 | 来源 | 注意 |
|---|---|---|
| `index.files` | `ui index` 输出 | 可索引文本文件数，不是目录里所有文件 |
| `index.symbols` | `ui index` 输出 | 保守前缀匹配，不是 AST 符号表 |
| `index.indexedLines` | manifest 求和 | 包含 README/docs；这是本框架的“行数”口径 |
| `index.durationMs` | 墙钟 | 含进程启动；同一机器内比较 |
| `run.rc` / `run.reason` | 子进程 / journal `turn/end` | rc=0 但 reason≠done 仍算失败 |
| `run.tokens` | 状态行 `tok N/`；取不到才用 journal 快照求和 | 报告写 `tokensSource` |
| `run.toolCalls` | journal 最终 `turn/end`；回退状态行/tool/start | 不含被拒绝的调用 |
| `run.durationMs` | 墙钟 | 包含模型与工具时间 |
| `changedCount` / `changedFiles` | 运行前后快照 | size+mtime 口径；touch 也可能算变化 |
| `verify.ok` | `ui verify` 输出 | 哈希链 + 无未完成 turn/tool |
| `expect.passed` | `bench_repos.py` 判定 | `null` 表示没有 expect，不是通过 |
| `taskVerify.rc` | 任务自己的命令 | 只有定义了 `verify` 才有 |

报告不把上面任何一项折算成单一“总分”。不同仓库、不同任务、不同 provider 之间
不能直接横向比较。

## 6. 怎么避免把 dry-run 当真实结果

看报告先看三个字段：

```json
{
  "mode": "dry-run",
  "dryRunReason": "未提供 provider（--provider-cmd / --mock / --provider）",
  "model": {"kind": "none"}
}
```

- `mode=dry-run`：没有 `ui run`，每个任务的 `run.skipped=true`、`status=dry-run`，
  `expect.passed=null`。报告顶部有“没有执行模型”的警示。**不能引用它的 success/failed。**
- `mode=real` + `model.kind=scripted`：`--mock` 真的跑了工具，但模型是剧本；
  只能证明管线可用，不能证明模型能力。
- `mode=real` + `model.kind=external`：真正的 provider 结果。必须同时归档
  repo commit、ui 版本、provider 名称/模型、任务文件 sha256、运行时间与报告文件。

占位符 `REPLACE_ME/...` 的仓库在报告里是 `skipped`，`index=null`，不会生成任何结果。
越界路径（相对路径用 `..` 逃出 repos 文件目录）、不存在路径、空目录是 `invalid`，
工具继续处理其他仓库并最终退出 2；这表示清单有问题，不是评测失败率。

## 7. 外部用户试用记录（字段）

正式对外说“有人用过”之前，至少保存以下字段；敏感项需用户书面同意后才能公开：

- 日期、试用者/组织（可匿名）、同意公开的范围；
- 仓库 URL、**固定 commit**、clone 方式、`dirtyFiles`（应为 0）；
- 任务文件路径与 sha256（`promptSha256` 每任务都在报告里）；
- ui 版本（`ui --version`）、操作系统/架构；
- provider 与模型名（不记录 key、不记录 provider 命令原文）；
- 超时设置、开始/结束时间、总耗时；
- 每任务：status、run rc/reason、tokens、toolCalls、changedFiles、verify rc、
  expect checks、任务 verify rc；
- 花费（token 单价 × tokens；单价由用户提供，不猜）；
- 失败与阻塞：模型错误、预算耗尽、超时、权限/网络问题；
- 报告文件路径与 sha256（`report.json` 本身可校验）；
- 复现命令（不含 key）与用户反馈；
- 结论边界：单次、单机、非统计显著，不与榜单比较。

## 8. 当前结果

| 项目 | 状态 | 可引用的数字 |
|---|---|---|
| 10 个真实仓库 × 12 任务 | **未运行** | 无 |
| 12 个合成 fixture × dry-run/index 自测 | 已通过（`tools/verify_bench_repos.sh`） | 12 个索引、144 条 dry-run 任务行；**不是真实结果** |
| provider 真实长跑 | 未做 | 无 |

自测命令：

```bash
bash tools/verify_bench_repos.sh
```

它验证：报告结构、12 个 fixture 的 index 指标、占位符 SKIP、provider 命令不落盘、
`..` 越界/空目录/缺失路径被拒绝且不终止其他仓库，以及 `--provider-cmd` 确实经
`UI_MODEL_CMD` 传给子进程、假 provider 失败被隔离为任务失败。
它**不调用真实 provider**（第 6 项只用一个立即退出的本地假命令，不联网）。

## 9. 已知边界

- 示例任务偏“仓库理解”，区分度低；真实评测需要仓库专属任务与可执行 verify。
- `changedFiles` 是 size+mtime 快照，不是 git diff；大仓库扫描有 20 万文件上限。
- `tokens` 是 UI 账本口径；不同 provider 的 usage 上报可能不一致。
- 每个任务只跑一次，没有多 seed、没有置信区间；不能据此宣称“模型 A 比 B 好”。
- `ui index` 的 symbol 提取是保守前缀匹配，不解析 AST，不能当代码理解能力指标。
- 框架不与 SWE-bench、Terminal-Bench 等榜单对齐，不承诺可比性。
- 真实任务可能修改仓库；正式评测应使用一次性 clone，跑完丢弃，不要在原仓库上反复跑。
