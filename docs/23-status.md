# 完成度快照（2026-11-28 快照 · 0.3.0）

> 本文档的**硬指标**由 `bash tools/status_doc.sh` 生成（2026-11-28 快照 · 0.3.0），不要手改；
> 分项完成度与「仍未解决」是人工判断，不由脚本生成。
> 数字来源：`tools/count_tests.sh`、`tools/debug.sh` 的 `run` 行、`docs/*.md` 计数与 git。

## 硬指标

| 指标 | 数值 |
|---|---|
| Rust 代码 | **24528 行** core + **18024 行** cli（零第三方依赖） |
| 测试 | **475 个**（5414 行测试代码） |
| 门禁 | **57 项**（`bash tools/debug.sh`） |
| 工具 | 13 个 + MCP + 插件网关 |
| 文档 | **60 篇**（另有 `tools/README.md` 索引） |
| 提交 / 标签 | 272 个 / 18 个标签（最新 `v0.2.2`） |
| CI | GitHub Actions 三个 job：`linux`（57 项门禁，含文档一致性与隐私两档）+ `windows`（交叉编译）+ `windows-runtime`（Windows 实机进程树验证） |

## 分项完成度

标准是**"一个陌生人能不能直接用/信"**，不是代码量。

| 领域 | 现状 | 完成度 |
|---|---|---|
| **内核与契约** | 崩溃不丢、并发安全、可重放；多写者竞态已修；**浸泡后加了内存上限** | **96%** |
| **审计与可复现** | 哈希链 + 独立校验器 + 8 场景攻击包 + 实验包对账；**verify/replay 已流式（内存 O(1)）** | **93%** |
| **会话** | CLI/Web 多会话、历史隔离；缺删除/搜索/分页 | **85%** |
| **权限边界** | 策略 + realpath + 观测式效果 + **Linux 读侧最小根与 Landlock 写沙箱**（fail closed）；Windows 无对等机制 | **92%** |
| **隐私与落账** | 落账前脱敏门 ＋ 来源扣留 ＋ 两个落盘目录（0600/0700）＋ 保留期与 run 边界自动清 ＋ **网页端同门脱敏**（模型 delta 原文不发布 / Journal 与 Hub 的助手、用户、工具摘要、工具全文和回放共用跨事件状态 / 快照）＋ `read.private` 审批与 `provesRead:false` 留痕 ＋ **Linux 隐私路径遮罩**；硬链接别名与 Windows 仍是边界 | **92%** |
| **多 provider** | 中继/DeepSeek 都能用；**前沿模型已能真正调工具** + 失败重试 | **78%** |
| **工具面** | 内置清单只有一份：`ui-core/src/tools.rs:864` 的 `registry()`；Windows 无 grep 时退化；无 LSP | **70%** |
| **多智能体** | 拆解→隔离→验收→不自动落地；缺 DAG/仲裁 | **70%** |
| **工程化/分发** | CI 三 job、发布包 + sha256 自校验、bootstrap/preflight/密钥扫描 | **70%** |
| **文档** | README（使用者向）+ `docs/` 近六十篇设计笔记；隐私与沙箱的说法由 `tools/verify_docs.sh` 对着代码查 | **70%** |
| **前端** | 四视图 + 会话 + 实验室 + 鉴权；无移动端/i18n | **60%** |

**综合：自用原型 ≈ 82%；可交付 ≈ 62%。**

## 这一轮补上的

| 项 | 之前 → 现在 |
|---|---|
| CI | 无 → **三 job 全绿**（Linux 门禁 + Windows 交叉编译 + Windows 实机运行时） |
| 版本 | 无 → `ui --version`（commit + 构建日期）→ `v0.1.1` |
| 体积 | 15 MB → **1.9 MB** |
| 可构建性 | 只有本机能构建 → 干净 clone 可构建（路径/依赖/端口全部显式） |
| 发布包 | 无 → tar.gz + sha256 + **解压重算自校验** |
| 前沿模型 | 只能聊天 → **能调工具、能跑实验**（3 seeds 跑出 0.96±0.016） |
| 实验包 | 只在 mock 下验证 → **真模型下验证**，并抓出 2 个真 bug |
| 测试 | 131 → **133**（新增的 lab 回归测试验证过"旧代码下会红"） |
| 隐私 | 日志里存明文 → **落账前脱敏 ＋ 来源扣留 ＋ 落盘原文有生命周期**（`ui spill`） |
| 控制面 | 取消/超时/回滚静默 → **每一步都有具名事件**（`turn/skipped`、`apply/*`、`approval/answered.via`、`privacy/*`） |
| 读隐私路径 | shell 读取不可观测 → **`read.private` 效果 + 默认 ask + `provesRead:false` observed + Linux 读侧路径遮罩** |

## 仍未解决的

| 缺口 | 严重度 | 说明 |
|---|---|---|
| Windows 无沙箱 | 中 | Linux 有读侧最小根与 Landlock；Windows 仍是"可执行程序"级信任 |
| 读侧视图覆盖范围 | 中 | Linux 默认遮住隐私/敏感路径，但硬链接同 inode 别名不在路径防护范围；Windows 无对等读侧视图。网络默认不限（`UI_SANDBOX_NET=1` 可禁 TCP） |
| 中继抖动 | 中 | 6 次调用里 3 次 `fetch failed`（已加 3 次重试，仍会失败） |
| 浸泡覆盖 | 中 | 2h soak 与 **0.3.0 基线 24h soak**（8580 轮，0 失败）都已通过；审批卡 P1 修复后的最终 v2 RC 已重新冻结并启动累计活跃 24h，尚未完成（见 [docs/48](48-soak.md)） |
| 真实评测规模 | 中 | 已有 5 个只读 pilot 与 3 个 Flask 修改任务；正式 10 仓库 × 12 任务已绑定最终 v2 RC 与真实 Qwen3 provider 运行，尚未完成（见 [docs/56](56-real-repos.md)） |
| 会话缺删除/搜索/分页 | 中 | 会话多了不好用 |
| 记忆的检索质量与仲裁 | 中 | `ui remember`/`forget`、TTL、来源与注入留痕（`memory/used`）、确定性检索都已在；缺语义排序与相互冲突时的仲裁（见 docs/30） |
| 无 LSP | 低（有意） | 与可审计冲突小，但成本高 |
| CHANGELOG 只覆盖已打 tag 的版本 | 低 | 0.2.2 及之前由 `tools/changelog.sh` 生成；0.3.0 在 CHANGELOG 里标为 **unreleased**，打 tag 后重跑生成器补段（见 docs/34 §5） |
| 脱敏是模式匹配 | 中 | 无密钥形状的机密会照原样进日志；`ui secrets` 事后能扫出来 |

## 一句诚实的话

**测试数量**（`bash tools/count_tests.sh` 实跑）——数字会变，以脚本输出为准；
门禁是 `bash tools/debug.sh` 那一串 `verify_*.sh`，CI 在 Linux 与 Windows 交叉编译上每推必跑。
这些是自动化能保证的部分。

Linux 读侧最小根 + Landlock 写沙箱，shell/插件/MCP 执行前后有 `effect/observed` 实测改动。
网页端展示层的脱敏也在 0.2.2 补上了（SSE/快照/工具全文库走同一道门）。Phase 1-B 把
shell 命令串里的隐私路径记为 `read.private`（默认 ask），执行后明确 `provesRead:false`；
Linux 读侧视图现在按路径遮住隐私项，失败即拒绝执行。Windows 与硬链接别名仍不受这层
保护，不能把审批留痕或路径遮罩说成 inode 级读取证明。`skill` 自动发现已收敛到
`skills/`，目录、文件与软链真实路径都过统一隐私/敏感名判定。
