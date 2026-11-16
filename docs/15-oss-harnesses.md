# 开源 Harness 全景与我们的位置

数据来源：
[headless-harness-bench](https://github.com/aaronjmars/headless-harness-bench)（2026-10-09 快照，
9 个 harness，静态审计 /81 + 契约测试 /21）、
GitHub Search API（2026-11-16 抓取）、
[futureagi](https://futureagi.com/blog/best-agent-harness/)。

## 一、被同一个基准量过的 9 个

基准考的不是"模型多聪明"，而是**另一个程序能不能把它当子进程驱动**：
传 prompt、工具白名单、system 追加，解析结构化输出，隔离密钥，超时能杀。
这正是"harness"该被衡量的方式，也正好是我们的场景。

| Harness | 仓库 | 语言 | 分数 /81 |
|---|---|---|---|
| omp（Oh My Pi） | can1357/oh-my-pi | TS | **66.1** |
| **dsh（DeepSeek Harness）** | deepseek-ai/deepseek-harness | TS/Electron | **65.6** |
| nanocodex | gakonst/nanocodex | Rust | 62.5 |
| opencode | sst/opencode | TS | 61.6 |
| pi | earendil-works/pi | TS | 60.9 |
| fx | vercel-labs/fx | TS | 56.6 |
| flue（框架） | withastro/flue | TS | 54.9 |
| crush | charmbracelet/crush | Go | 52.1 |
| eve（框架） | vercel/eve | TS | 52.1 |

噪声约 ±3（单次静态评审），所以只能读档位，不能抠小数。
**DSH 的水平 = 65.6/81**，这就是我们要对齐的那条线。

## 二、另外几个值得看的（按 GitHub star，2026-11-16）

| 项目 | 星 | 许可 | 它是什么 |
|---|---|---|---|
| ECC | 304k | MIT | 不是 harness，是**给别的 harness 做优化**（Claude Code / Codex / OpenCode 的技能、记忆、安全层） |
| deer-flow（字节） | 85k | MIT | "长时程 SuperAgent harness"：沙箱 + 记忆 + 工具 + 技能 + **子代理 + 消息网关** |
| ruflo | 82k | MIT | 多智能体蜂群 + 向量记忆 + 联邦，主打编排 |
| learn-claude-code | 80k | MIT | 教学用 nano harness（"Bash is all you need"），适合读架构 |
| omnigent | 11k | Apache-2.0 | agent 治理/编排平台，带沙箱 |
| munder-difflin | 9.6k | MIT | 在本地开"agent 办公室"，复用你的 Claude/Codex 订阅 |
| ZCode（智谱） | 8.7k | Apache-2.0 | 厂商自研 harness |
| deepsec（Vercel） | 8.4k | Apache-2.0 | **安全专用** harness：拿 coding agent 找漏洞 |
| PI-Desktop | 8.0k | LGPL-3.0 | Electron + **Rust host core** + 插件 |

> 星数只作参考：榜首那个 304k 明显有刷量嫌疑，别当能力指标。

## 三、九个 harness 的共性

1. **几乎全是 TS/JS**（唯一两个非 JS：crush 用 Go、nanocodex 用 Rust）——
   我们是 Rust 内核 + 零依赖，这在这个列表里是少数派；
2. **都在抢"可被程序驱动"**：结构化输出、工具白名单、超时取消、密钥隔离；
3. **沙箱是标配**：容器 / 本地沙箱 / 工作区隔离，各有实现；
4. **技能 + 记忆**是 2026 年新增的主战场（ECC、deer-flow、ruflo 都堆这块）；
5. **审计几乎没有**：9 个里没有一个把"决策级日志 + 哈希链"当卖点。

## 四、我们对着这个尺子自己量一遍

基准的 Tier-1 是 8 个加权类别、42 条标准，我们没跑过（需要把源码喂给它的 rubric）。
但可以先用它的**考核方向**自评：

| 维度 | 我们的现状 | 判断 |
|---|---|---|
| 可被程序驱动 | `ui run --task` + JSON 事件 + 退出码约定 | 强 |
| 结构化输出 | 日志 JSONL + SSE + `--json` | 强 |
| 工具白名单 | 效果集策略（比白名单更细） | 强 |
| 超时/取消 | 进程组 kill + Cancel 令牌 + `cancel_hard` | 强 |
| 密钥隔离 | `secrets/` 默认 deny + 不在提示里 | 强 |
| 沙箱 | 工作区级隔离副本；无容器 | 中 |
| 会话/多轮 | 单会话（**最弱**） | 弱 |
| 工具面 | 7 个 + MCP tools | 弱 |
| 记忆/技能 | 技能有，记忆无 | 中 |
| 审计 | 哈希链 + 血统 + 结论对账 | **最强，别人没有** |

粗估：**契约与可驱动性上我们接近榜首，能力面（工具/会话/记忆）上明显落后**。
如果按 81 分制，我估 55–62 区间——落在中游，正好是"内核强、外围薄"的形状。

## 五、该怎么用这份基准

1. **别自己发明标准**：直接采纳它的 8 个类别做自评表，差距就有了可比的坐标；
2. **短板顺序不变**：会话 → 工具面 → MCP 完整 → 子代理 UI；
3. **优势要主动量化**：它的 42 条标准里没有"可审计"，我们可以补一条并给出证据
   （哈希链、`audit --as` 重判、实验包对账）——这是唯一能让我们区别于 9 个的地方。

## 六、一句结论

这个赛道已经很挤：9 个成熟 harness，6 个是 TS，两个是框架，全部在做"可被驱动"。
**我们不可能靠"功能更多"赢**——但"每一步都能被验证"这条，现在没人做。
