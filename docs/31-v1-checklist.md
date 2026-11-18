# 31 · v1.0 的验收门槛

这份文档定义**什么叫"到 v1"**，以及每一条怎么验证。
判定原则只有一条：**能被第三方用命令复现的，才算做到**；写在文档里但验证不了的，不算。

## 一、v1 的定义

> 一个单机、零依赖、可审计的 agent harness：任何一次运行都能回答
> "做了什么、为什么、改了什么、花了多少"，并且在 Linux 与 Windows 上都能构建、能跑。

不是 v1 的东西（明确排除）：多机调度、云端同步、向量检索、Web 账号体系、插件市场。

## 二、硬门槛（全部必须通过）

| # | 门槛 | 验证命令 | 现状 |
|---|---|---|---|
| R1 | 全量测试通过 | `bash tools/test.sh` | ✅ 通过（数量以 `tools/count_tests.sh` 输出为准；另有 ignored 的吞吐探针） |
| R2 | 门禁通过 | `bash tools/debug.sh` | ✅ 全部通过（项数以脚本输出为准） |
| R3 | **干净检出的 HEAD 可构建** | `debug.sh` 的「干净检出的 HEAD 可构建」项（临时 worktree + 独立 target） | ✅ |
| R4 | CI 在 Linux 与 Windows 交叉编译上通过 | `tools/ci.yml` 两个 job | ✅ |
| R5 | 日志链可校验、可回放 | `ui verify` / `ui replay` | ✅ 30 万条 549 ms / 3.4 MB |
| R6 | 七类契约事件齐全且各有测试 | 见 `docs/02-design.md` §契约 | ✅ |
| R7 | 信任边界有隔离（计划内）与观测 | `ui doctor` 的信任边界块 + `verify_sandbox.sh` | ✅ Linux Landlock / Windows Job Object |
| R8 | 无"静默丢失/静默覆盖"类已知缺陷 | 见第四节 | ⚠️ 见 U1 |
| R9 | 文档与代码一致（版本、测试数、命令） | 本文表格 + README | ⚠️ 收尾时统一 |
| R10 | 发布产物可复现 | `bash tools/release.sh` + 校验和 | ✅ 1.1 MB tar.gz，自校验通过 |
| R11 | 长任务无泄漏 | `bash tools/soak.sh --steps 1200 --minutes 0` | ✅ 10712 事件 / 34 s / 内存增长 **1.40x** / 链完整 / 孤儿进程 0 |

## 三、能力门槛（v1 必须有）

| # | 能力 | 判断标准 | 现状 |
|---|---|---|---|
| C1 | 长任务可靠 | 崩溃后 `ui resume` 能接上未完成调用 | ✅ |
| C2 | 上下文与成本可控 | 压缩 + 预算 + 每轮 token 记账 | ✅ |
| C3 | 权限下高自由度 | 策略（allow/ask/deny）+ `--accept` 门 | ✅ |
| C4 | 并行与隔离 | `ui team` / `workflow --isolate` + 漂移检测 | ✅ |
| C5 | **可审计的知识** | 卡片绑定源码 + `knowledge/use` 进链 | ✅ v0.1.15 |
| C6 | **记忆** | `ui remember` → 检索注入 → `memory/used` 可审计 | ✅ v1.0.0（P1 人工写入） |
| C7 | 可复现实验 | `ui lab new/pack/verify/table/claim` | ✅ |

## 四、已知限制（v1 允许带着发布，但必须写明）

| # | 限制 | 影响 | 为什么可以接受 |
|---|---|---|---|
| U1 | **子代理删除的文件不会被合并** | 父工作区保留旧文件 | 删除是不可逆操作，宁可不做；`apply` 报告会列出删除但不自动执行（D1 已完成） |
| U2 | 知识卡 `source_sha` 为空 = UNBOUND，默认不算 STALE | 清空 sha 可绕过默认审计 | `--strict` 会红；UNBOUND 在输出里明示 |
| U3 | 知识预取只在 `ui run` | `chat/workflow/team` 的子任务不预取 | 子任务上下文由父决定，v1 不做 |
| U4 | 命中不打分（score 恒 null） | 多于 3 张卡时按 id 取，不是按相关性 | 确定性优先：宁可可复现，不要不可解释的相似度 |
| U5 | Windows 无文件系统隔离（只有 Job Object 资源限制） | 子进程能写工作区外的文件 | 已在 `docs/25` 写明，v1 不做低完整性令牌 |
| U6 | shell 间接读密钥仍是 GAP | `cat secrets/x` 拦不住 | 见 `docs/22` 攻击表，v1 不假装能拦 |
| U7 | `memory/used` **双写**（主日志 + 记忆链） | 两处各自可校验，但不是唯一事实源 | 主日志给 replay/verify，记忆链给 hits 派生；P2 再决定是否收敛成单链 |
| U8 | 记忆只有 P1（人工写入） | 模型不能自己写记忆、没有候选/确认机制 | 这是**有意**的：自动写记忆最容易污染审计，P2 才做且必须人工确认 |

## 五、v1.0.0 发布前待办（已全部完成）

| # | 事项 | 负责人 | 状态 |
|---|---|---|---|
| D1 | `apply` 把"子代理删除的文件"列进报告（不自动删） | 内核 | ✅ `isolate::diff` + `delegate/end.deleted` + apply 报告 + 集成测试 |
| D2 | 记忆 P1（存储/检索/注入/审计 + 6 个测试） | 并行 agent | ✅ 194→195 个测试全绿 |
| D3 | Windows 原生冒烟（exe 实跑 + 中文路径 + Job Object） | 并行 agent | ✅ 三项全过；顺带修掉"Windows 编不过 + 构建脚本假绿" |
| D4 | README / docs/23 的数字与版本统一（195 个测试、46 项门禁） | 文档 | ✅ |
| D5 | `docs/29` K1 措辞修正（卡源校验是 `ui knowledge check`，不是 `verify`） | 文档 | ✅ |
| D6 | 发布：`Cargo.toml` → 1.0.0、CHANGELOG、tag `v1.0.0`、release 产物 | 发布 | ✅ v1.0.0 / v1.0.1 已发布 |

> V1（外部锚定：`ui verify --expect-head` + 打包时记录头部哈希）**不在 v1 范围内**，
> 保留为 **v1.1 待办**，设计说明见 [32-auditable-memory-theory.md](32-auditable-memory-theory.md) §2.4。

## 六、发布动作（D1–D6 全绿之后）

```bash
bash tools/test.sh          # 必须 == 测试通过 ==
bash tools/debug.sh         # 必须 0 failed（项数以输出为准）
bash tools/changelog.sh     # 重新生成
bash tools/release.sh       # 产出 tar.gz + sha256 + 自校验
bash tools/tag.sh v1.0.0 <sha> "ui 1.0.0"
```

发布后必须验证：`shields.io` 的 CI 徽章为 passing，且 `git ls-remote --tags` 里 `v1.0.0`
指向的提交 == 远端 main == 本次 CI 通过的提交。

## 七、事故记录：半成品提交（两次）

v1 之前出现过两次**同一个错误**：提交时把并发 agent 正在改的文件一起带上了，
于是提交树编译不过（本地工作树是绿的）。

| 次数 | 提交 | 症状 | 怎么发现的 |
|---|---|---|---|
| 1 | `597ebe2` | `main.rs` 有 `mod knowledge;`，但四个新文件还是 untracked | 实现 agent 在交付时主动指出 |
| 2 | `505fcca` | `prompt.rs` 已加 `memory_text` 参数（8 个），但 `agent.rs` 的调用处（7 个）没提交 | 门禁第 43 项「干净检出的 HEAD 可构建」直接失败 |

两次都让 CI 红了一次。根因不是技术，是**流程**：

> 本地 `cargo build` 只证明**工作树**能编译，不证明**提交**能编译。

### 现在的规则（已落成门禁）

1. **推送前必须跑 `bash tools/debug.sh`**，其中「干净检出的 HEAD 可构建」一项会从 `HEAD` 建临时 worktree
   重新构建——这是唯一能发现"提交不完整"的检查。
2. 提交时**只 `git add` 自己完整负责的文件集合**；如果某个文件里有别人的改动，
   要么等对方提交，要么把它的依赖一起提交（保证自洽）。
3. 并发 agent 的场景下，**不要**用 `git add -A` / `commit -a`。

新增的四项门禁（加入时的编号 43→46；之后门禁继续增加，编号会变）：

| # | 检查 | 为什么需要 |
|---|---|---|
| 43 | 干净检出的 HEAD 可构建 | 抓"提交不完整" |
| 44 | Windows 构建 fail-closed（失败必须非零） | 抓"构建失败了脚本还退出 0、打印旧产物" |
| 45 | Windows target 下测试代码可编译 | 抓"只在 Linux 编、Windows 编不过的测试" |
| 46 | 知识卡来源绑定（bind / STALE） | 抓"知识卡与源码脱钩" |
