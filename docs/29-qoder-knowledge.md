# 29 · Qoder 知识引擎：哪些是机制，哪些是话术

调研日期：2026-11-17　｜　对象：Qoder（阿里，IDE / CLI / 企业版）"知识引擎（Knowledge Engine）"
方法：6 轮中英文检索（18 条 query）+ 20 余次原文抓取，只使用公开资料。
**限制：我们没有安装或实测 Qoder，本文没有一手实验数据。** 所有"机制"都是对公开文档/博客的复述与交叉核对，所有"效果"都是转述厂商自述。

---

## 一句话结论

Qoder 的知识引擎不是"更强的 RAG"，而是把**知识生产**（RepoWiki / Knowledge Cards / Memory 三件产物 + 增量维护 + 团队共享）放在**检索**之前的一套工程组合；产物形态与调用方式**可查**（官方文档 + 博客 + 媒体多方一致），但**所有效果数字都只有厂商一个来源、口径互相矛盾、无法复现**。对我们真正有价值的不是向量索引或知识图谱，而是"知识是带来源、版本、生命周期的产物"这个判断——而我们恰好有日志链，可以把它做成 Qoder 做不到的**可审计知识**。

---

## 一、事实分级口径

| 级别 | 定义 | 判据 |
|---|---|---|
| **A 多源确认** | ≥2 个可独立访问的来源在事实上一致 | 注明是否同厂商（官方文档多页也算同厂商） |
| **B 单一来源** | 只有一个来源 | 官方文档只此一处，或仅一篇第三方文章 |
| **C 宣称·未验证** | 只有营销页/新闻稿/厂商自评，无方法或无法复现 | 含"厂商博客给了方法但没给数据" |
| **D 查不到** | 公开资料没有，或只有推测 | 直接写"查不到"，不补全 |

**先给一个总体判断**：能被两个以上来源确认的，几乎全是"产品行为"（有什么产物、怎么调用、存哪里）；**凡是"提升了多少"的数字，没有一个达到 A 级**。

---

## 二、Qoder 宣称了什么（原文 + 链接）

### 2.1 "越用越聪明"的原文

官方中文企业页原话：

> **"企业知识引擎让 Agent 越用越聪明。"**
> "团队日常使用中积累的经验自动沉淀为企业知识，Agent 在后续响应和操作中直接调用。新人即刻获得专家水平的回答，重复踩坑大幅减少……知识通过代码和对话两条路径自动沉淀。"
> —— [Qoder 企业版](https://qoder.com.cn/enterprise)

英文博客结尾的对应表述：

> "The longer you use it, the better it gets."
> —— [How we built a self-evolving memory system for Qoder](https://blog.qoder.com/qoder-memory-evolution)，2026-04-23

注意：**"聪明"没有被定义，也没有度量**。官方给的是三条替代指标（代码保留率 / token / 轮次），见 2.3、2.4。

### 2.2 知识引擎是什么（官方定义）

> "Knowledge includes three types of knowledge sources: **Repo Wiki** … **Knowledge Card** … **Conversation Memory (Memory)** … These three types of knowledge flow into the same engine, requiring no manual organization, and the Agent will automatically invoke them when needed."
> —— [docs: Knowledge Overview](https://docs.qoder.com/user-guide/knowledge-engine/overview)

> "It is **not a documentation library** in the traditional sense—it is a **project-understanding layer** built specifically for AI coding tasks."
> "code search locates facts, while the Knowledge Engine **expresses engineering semantics**."
> —— [A Self-Iterating Knowledge Engine for AI-Native Software Engineering](https://blog.qoder.com/qoder-knowledge-engine)，2026-07-21

### 2.3 1.0 发布会的三个数字

> "知识引擎功能上线后，用户**代码保留率提升 11%**，输入 **Token 消耗大幅降低 40%**，**对话轮次减少 33%**。"
> —— [极客公园，2026-05-15](https://www.geekpark.net/news/364252)

同一段话出现在[品玩](https://www.pingwest.com/w/313749)、[亿欧](https://www.iyiou.com/briefing/202605151908087)、[站长之家](https://www.chinaz.com/ainews/28030.shtml)等多家媒体——**它们同属一份新闻稿，不是独立来源**（见第四节对账）。

### 2.4 官方博客里的另外三组数字（互相不一致）

| 出处 | 任务效果 | token | 其他 |
|---|---|---|---|
| [Engineering Knowledge Engine](https://blog.qoder.com/engineering-knowledge-engine)（2026-03-12） | 任务完成分 **+12%** | **-14%** | 检索 F1 +21%；在线 A/B 代码保留率 **+1.9%**（1000+ 文件仓库 +2.2%），复杂任务轮次 -7.1% |
| [Knowledge Engine](https://blog.qoder.com/qoder-knowledge-engine)（2026-07-21） | SWE-bench Pro 40 案例，30.2→**38.4（+27.1%）** | **-6.1%**（3110K→2920K） | 轮次 35.5→31.7，CV 11.1%→6.4% |
| [Memory](https://blog.qoder.com/qoder-memory-evolution)（2026-04-23） | 内部记忆基准 **87/77/50**（核心 3 场景）/ **85/40/28**（全 5 场景） | Q&A 场景 **-40%** 输入 token | 个性化场景留存 **+11%**，复杂任务轮次 **-33%**，不满意 -22% |

---

## 三、可验证的机制拆解

### 3.1 全景：两路上游 → 三件产物 → 一条维护链

```
代码仓库（模块结构/依赖/约定）  ┐
历史会话（偏好/决策/经验）      ┴→ 知识引擎 → RepoWiki（人读）
                                    ├→ Knowledge Cards（Agent 读）
                                    └→ Memory（跨会话偏好/经验）
                    治理：用户纠正 · 增量更新 · 共享 · 导入导出
```

官方定义的三件产物分工很明确（[blog](https://blog.qoder.com/qoder-knowledge-engine)）：

- **RepoWiki**：面向人阅读与导航的叙事型文档；
- **Knowledge Cards**：面向 Agent 消费的**高密度、结构化**知识单元（模块职责、架构设计、技术栈、编码约定、启动命令、模块间关系）；
- **Memory**：历史交互中形成的偏好、项目经验、踩坑记录。

**这是整份材料里最值得记的一个设计判断**：给 Agent 的上下文不该是"给人看的文档"的截断，而应是另一种表示（"Wiki 面向人类阅读习惯，直接喂给 Agent 它还得重新抽取模块边界与调用关系"）。Qoder 自己也是这么解释 Card 存在的理由的。

### 3.2 代码库索引：**云端向量化**

官方文档写得最直白：

> "codebase indexing **uploads only the necessary code files** … **to generate vector data in the cloud**. The uploaded code files are **destroyed after vector generation**. **Only vector data is retained in the cloud**."
> —— [docs: Codebase indexing](https://docs.qoder.com/user-guide/indexing)

其余可查的硬边界：

- 支持 **10 万文件**；**<1 万文件默认开自动索引**，更大的仓库需手动开；
- 忽略规则：`.gitignore` + `.qoderignore`；
- "不保存源码"仅指不长期保留，**向量化过程需要把代码文件上传**。

对照另一条数据路径（容易忽略的矛盾点）：

> "**Repo Wiki generation runs on your local client** … **No full-codebase upload** … 默认关闭，增量更新可自行开关。"
> —— [docs: Repo Wiki / Data Security](https://docs.qoder.com/user-guide/repo-wiki)

也就是说：**Wiki 是本地生成，索引是云端向量化**。两条通道是否共享数据、向量是否可反推代码——官方文档没有说明（见第八节）。

### 3.3 仓库知识图谱：三张图，不是一张

官方博客把"图"拆成三种用途，这点比"我们有知识图谱"的口号具体得多（[eke blog](https://blog.qoder.com/engineering-knowledge-engine)）：

| 图 | 内容 | 解决什么 |
|---|---|---|
| **Code Graph** | 代码实体间的调用、引用、继承、实现 | 符号级关系：问"登录怎么实现"时能带出 token 服务、session、相关模块 |
| **Commit Graph** | 提交历史 + **LLM 增强过的 commit message** | 意图层：`Query → Commit Message (intent) → Code` 两阶段检索，回答"当初为什么这么写" |
| **Card Graph** | 知识卡节点 + 边（包含/依赖/关联/**版本演化**） | 模块级语义：跨模块改动时给出"这个模块怎么和别人协作" |

> "edges express containment, dependency, association, or **version evolution** between nodes."
> —— [KE blog](https://blog.qoder.com/qoder-knowledge-engine)

我们的判断：**Commit Graph 是三者里最聪明的一个**——它承认"向量检索回答不了为什么"，于是把 commit message 当作"意图与实现之间的中间表示"。同时它也是最危险的一个（见 N3）。

### 3.4 知识卡的生命周期（四段）

官方给的是一条完整流水线（[KE blog](https://blog.qoder.com/qoder-knowledge-engine) + [docs](https://docs.qoder.com/user-guide/knowledge-engine/knowledge-cards)）：

1. **扫描与生成**：模块规划（怎么切知识节点）→ 知识合成（职责/架构/技术栈/约定/命令）→ 关系抽取；
2. **图谱构建**：卡片为节点、关系为边，形成可查询的项目知识图；
3. **治理与共享**：`/knowledge` 命令（生成/修改/补充/重写）、`wiki_plan.yaml` 预生成配置（页面白名单、include/exclude、写作意图）、**人工修改被标记保护，不被下次自动更新覆盖**；
4. **Agent 消费**：任务前多路召回 + 任务中按需查询；"不只返回孤立卡片，而是卡片 + 关联关系"。

落地形态是可 Git 化的文件（这点很重要）：

```
<repo>/.qoder/repowiki/
  wiki_plan.yaml
  knowledge/zh/…  knowledge/en/…   # 卡片（Markdown + YAML）
  content/  meta/repowiki-metadata.json
```

导入是"替换对应分支+语言"，导出是"覆盖对应语言目录"——**有明确的覆盖语义，但没有 diff/合并语义**（[docs](https://docs.qoder.com/user-guide/knowledge-engine/knowledge-cards)）。

### 3.5 记忆 / 偏好学习：五类、五触发、六路召回

官方记忆博客的细节密度最高（[memory blog](https://blog.qoder.com/qoder-memory-evolution)）：

- **存什么**：三个主体（agent / project / user）→ **5 大类 26 子类**（user preferences、project info、development conventions、lessons learned、tools & skills）；**每个子类一个独立子图**，避免"tab vs space"污染"数据库 schema"的检索；
- **何时写**：项目初始化、代码被采纳、会话结束、模式跨会话重复、用户显式指令——5 个触发点；
- **怎么写**："七步管线 + 六段式抽取 prompt"；一条原则：**"Don't hand to the model what engineering can solve."**（参数裁剪、元数据由工程侧补，模型只做语义抽取）；
- **怎么组织**：用户轨道（增删改查）+ 系统轨道（**遗忘打分**=容量/新近度/保留率/质量、失效清理、记忆网络重连、异常检测、经验提升、共享、会话结束时清工作记忆）；
- **怎么取**：意图识别（主/次/隐藏意图）→ **六路并行召回**（向量 / 全文 / 记忆图 / 模糊关键词 / 精确关键词 / 固定查询）→ 工具检索（浅/深/探索/直取）→ 重排（轻量 ranker / 大模型 / **RRF 兜底**）；
- **两个结构**：**Memory Graph**（关键词节点 + 记忆节点，加权边，语义激活 + 图激活的多跳扩散）与 **Topic Tree**（类别层级，纵向精确）；
- **自演化**：**元记忆**（监控 + 反思）+ 双飞轮（记忆数据飞轮 / 策略数据飞轮）。

官方还坦白了一个我们也会遇到的问题：

> "**Large models don't reliably use retrieval tools correctly.** They don't always call memory retrieval at the right moment, and they don't always construct the best query."
> 同文还否定了"把记忆做成虚拟文件让主 Agent 自己翻"的路线："The main agent won't pause a coding task to scroll through memory files looking for a note it doesn't know exists."

### 3.6 检索增强：Agentic Search 是"独立搜索代理"

> "stop making your reasoning model do search. Give it a search agent instead."
> 主模型只说"要什么"（意图层），搜索代理做策略（选工具/并行/由粗到细/迭代预算）与执行（语义、图、正则、文件、目录、读文件、Wiki 七类工具），**只把提炼结果送回主上下文**。
> —— [Agentic Search](https://blog.qoder.com/agentic-search)

官方 demo 称主模型 token 降 **30.2%**；离线 Agent Bench（45 个任务）另有数字。**单来源、厂商自测。**

工程上值得抄的是**职责分离**（检索不占推理模型的上下文预算），不是"再起一个 agent"本身——后者对我们意味着额外的 token、延迟和不可控性。

### 3.7 上下文工程与预算

- IDE 上下文 **200K**，压缩是**手动** `Compact Chat`；官方明确"压缩会影响生成质量，所以在对话早期和生成过程中禁用"（[docs](https://docs.qoder.com/user-guide/chat/smart-context-control)）。**自动触发的阈值策略查不到。**
- 知识注入分两个时机：任务前 **prefetch**（多路召回→过滤→组织→注入）与任务中 **in-flight**（按需查卡补细节）。官方实验显示 prefetch 单独用会有"锚定风险"（一次选不全），KE 的优势来自"前期组织 + 过程补充"（[KE blog](https://blog.qoder.com/qoder-knowledge-engine)）。
- 一句关键表述：**"What the Knowledge Engine increases is not context length, but the density of useful information at each execution stage."**
- 具体的知识注入预算：**查不到**（IDE 侧未公开）。唯一可见的相邻数字来自对 **QoderWork**（办公 Agent，非 IDE）的第三方逆向：Project 层 **40K token 预算**、top-6、critical/normal 两级、FTS5 trigram 索引（[腾讯云社区](https://cloud.tencent.com.cn/developer/article/2707349)）。**这是单一来源、非官方、且产品不同，不能当成 IDE 的事实。**

### 3.8 实验方法（这部分比数字可信）

给出方法最完整的是 KE 博客：从 **SWE-bench Pro 抽 40 个复杂案例**，5 种条件（无知识 / CodeGraph MCP / 工具检索 / Agent prefetch / Knowledge Engine），**每种独立跑 3 次**，报均值与**变异系数 CV**，指标含任务分、token、轮次、工具调用数、墙钟时间（[KE blog](https://blog.qoder.com/qoder-knowledge-engine)）。

方法本身是像样的（有对照、有重复、有方差）。问题在于：**没有原始任务集、没有评分 rubric、没有满分、没有脚本、没有第三方复现**。第三方 CodexQA 复述时也指出："文章没有写出这套分数的满分"（[CodexQA](https://codexqa.cn/en/articles/aliyun-qoder-knowledge-eval-zh)）——注意它是**复述，不是复现**。

---

## 四、效果数字对账（本节是全文最重要的发现）

### 4.1 发布会的 11% / 40% / 33%，能在更早的记忆博客里找到"分场景原值"

| 发布会在说 | 记忆博客在说什么（2026-04-23，早于 1.0） |
|---|---|
| 代码保留率 **+11%** | 个性化场景留存 **+11%**（"users come back more often"） |
| 输入 Token **-40%** | Q&A 场景输入 token **-40%**（"context reuse"） |
| 对话轮次 **-33%** | 复杂任务轮次 **-33%**（"first-time hit rate"） |

记忆博客自己强调过：**"We report per-scenario because memory's impact flips direction … A global 'tokens saved' metric would average those out and lie."**（按场景报，因为记忆的影响会反向；全局"省了多少 token"会把差异平均掉并**说谎**。）

而发布会把三个**不同场景**的极值并列成了"知识引擎上线后"的整体效果，且把"个性化留存"改写成"代码保留率"。

**这是我们的推断，不是官方承认**：三个数字与记忆博客的分场景 A/B 逐一吻合，时间上记忆博客在前，发布会未给出新方法。可信的解释是"分场景极值被合并表述"；官方没有回应这一点（**查不到**）。

### 4.2 三套口径彼此不能相加

- +12%（eke，2026-03）与 +27.1%（KE，2026-07）不是同一个基准：前者是"Qoder Agent Bench"自建集，后者是 SWE-bench Pro 40 例；
- token 降幅 14% / 6.1% / 40% 分别对应"整体 / KE 对照 / Q&A 场景"；
- 代码保留率 +1.9%（在线 A/B，全仓库）与 +11%（个性化场景留存）差一个数量级；
- 同一篇记忆博客里，同一个对照系统的分数先写 87、后写 85（"全五场景"），两组数（87/77/50 与 85/40/28）**没有解释差异**——口径内部也不完全自洽。

**结论：任何一个数字单独引用都会误导；它们各自只在自己的场景与基准内成立。** 官方从未把这些数字放进同一张表。

### 4.3 用我们的标准，这些数字过不了 `ui lab verify`

按 [09-research.md](09-research.md) 的硬规则"没有产物就没有结论"：Qoder 的 27.1% 没有任务集、没有评分产物、没有 `claims.json`、没有可重跑的脚本——**在我们自己的实验包里，它会被判为"结论在产物里找不到对应数值"（退出码 3）**。这不是说数字假，而是说它**不可审计**，与我们的交付标准不同频。

---

## 五、事实分级表

| # | 结论 | 分级 | 来源 | 独立性 / 备注 |
|---|---|---|---|---|
| F1 | 知识引擎 = RepoWiki + Knowledge Cards + Memory 的统一体，Agent 自动调用 | **A** | [docs overview](https://docs.qoder.com/user-guide/knowledge-engine/overview)、[KE blog](https://blog.qoder.com/qoder-knowledge-engine)、[极客公园](https://www.geekpark.net/news/364252) | 厂商文档 + 博客 + 媒体（媒体源自新闻稿，弱独立） |
| F2 | RepoWiki 面向人、Cards 面向 Agent，二者同源生成 | **A** | [KE blog](https://blog.qoder.com/qoder-knowledge-engine)、[docs cards](https://docs.qoder.com/user-guide/knowledge-engine/knowledge-cards) | 同厂商两页 |
| F3 | 卡片存 `.qoder/repowiki/knowledge/{zh,en}/`，可 Git 共享；企业版按 repo+branch 同步 | **A** | [docs cards](https://docs.qoder.com/user-guide/knowledge-engine/knowledge-cards)、[docs repo-wiki](https://docs.qoder.com/user-guide/repo-wiki)、[docs enterprise](https://docs.qoder.com/account/enterprise/knowledge-base) | 同厂商三页 |
| F4 | Memory 跨会话、分类、全局+项目作用域、可查看/编辑/删除 | **A** | [docs memory](https://docs.qoder.com/user-guide/knowledge-engine/memory)、[memory blog](https://blog.qoder.com/qoder-memory-evolution) | 同厂商 |
| F5 | 知识生成/更新消耗 Credits | **A** | [docs repo-wiki](https://docs.qoder.com/user-guide/repo-wiki)、[docs cards](https://docs.qoder.com/user-guide/knowledge-engine/knowledge-cards) | 同厂商 |
| F6 | Code Graph（调用/引用/继承/实现）+ Commit Graph（LLM 增强 commit message，两阶段检索） | **A** | [eke blog](https://blog.qoder.com/engineering-knowledge-engine)、[agentic-search](https://blog.qoder.com/agentic-search) | 同厂商 |
| F7 | Agentic Search：独立搜索 agent、意图/策略/执行三层、七类工具 | **A** | [agentic-search](https://blog.qoder.com/agentic-search)、[eke blog](https://blog.qoder.com/engineering-knowledge-engine) | 同厂商 |
| F8 | 代码索引是**云端向量化**：上传必要代码文件，向量生成后销毁，只留向量 | **B** | [docs indexing](https://docs.qoder.com/user-guide/indexing) | 仅官方一处 |
| F9 | 索引上限 10 万文件；<1 万默认自动索引；`.qoderignore` 可排除 | **B** | 同上 | 仅官方一处 |
| F10 | RepoWiki 在**本地**多 Agent 生成、不上传全库、默认关闭 | **B** | [docs repo-wiki](https://docs.qoder.com/user-guide/repo-wiki) | 仅官方一处；与 F8 的数据路径关系未说明 |
| F11 | 卡片支持 `/knowledge` 干预、`wiki_plan.yaml` 预配置、人工修改受保护 | **B** | [docs repo-wiki](https://docs.qoder.com/user-guide/repo-wiki) | 仅官方一处 |
| F12 | 企业知识采集**默认关闭**；客户端"同步团队知识"**默认开启**，云端版本**覆盖本地**；禁用后数据限期删除 | **B** | [docs enterprise](https://docs.qoder.com/account/enterprise/knowledge-base) | 仅官方一处，治理语义关键 |
| F13 | 记忆实现细节：5 类 26 子类、5 触发点、7 步管线、6 路召回、遗忘打分（容量/新近/保留/质量）、元记忆双飞轮 | **B** | [memory blog](https://blog.qoder.com/qoder-memory-evolution) | 厂商自述，无代码/无第三方 |
| F14 | 记忆在线 A/B：-22% 不满意、+11% 留存、-40% Q&A token、-33% 复杂任务轮次；85% 会话参与、60% 记忆有效率（3 天、top5 场景） | **B** | 同上 | 厂商自测，未给样本量与显著性 |
| F15 | 40 案例 ×3 条件实验：30.2→38.4（+27.1%）、token -6.1%、CV 11.1%→6.4% | **B** | [KE blog](https://blog.qoder.com/qoder-knowledge-engine) | 厂商实验；CodexQA 复述非复现 |
| F16 | Qoder Agent Bench：+12% 任务分、-14% token、检索 F1 +21%；在线 A/B 保留率 +1.9%/+2.2%、轮次 -7.1%；Memory +0.66% 保留、-27% 不满意 | **B** | [eke blog](https://blog.qoder.com/engineering-knowledge-engine) | 厂商自建基准，无定义 |
| F17 | 大仓库索引可能不全：用户报告 6,636 个文件"只索引了部分" | **B** | [Qoder 论坛](https://forum.qoder.com/t/repo-wiki-ai/2701/3) | 单个用户报告，无官方回应 |
| F18 | QoderWork（非 IDE）记忆逆向：FTS5 trigram、Project 层 40K 预算、top-6、critical/normal 驱逐、反思前备份/65% 保留率校验/`.memory_evicted.log` | **B** | [腾讯云社区](https://cloud.tencent.com.cn/developer/article/2707349) | 第三方逆向、单一来源、产品不同 |
| F19 | 1.0 发布会：保留率 +11%、token -40%、轮次 -33% | **C** | [极客公园](https://www.geekpark.net/news/364252) 等 | 同一新闻稿多站转载，**同源**；无方法 |
| F20 | "越用越聪明"、"新人即刻获得专家水平的回答" | **C** | [Qoder 企业版](https://qoder.com.cn/enterprise) | 营销页；"聪明"无定义 |
| F21 | 索引提速 5x、"95% 新开仓库 1 分钟内完成索引" | **C** | [eke blog](https://blog.qoder.com/engineering-knowledge-engine) | 无方法、无第三方 |
| F22 | "个人能力沉淀为组织能力" | **C** | [极客公园](https://www.geekpark.net/news/364252)、[企业页](https://qoder.com.cn/enterprise) | 采集机制存在（F12），"能力"不可测 |

**A 级 7 条，B 级 11 条，C 级 4 条。** 也就是说：机制层大体可查（多为厂商文档，可作为"设计参考"），效果层几乎没有独立证据（不能作为"决策依据"）。

---

## 六、对 UI harness 的借鉴清单

先对齐我们自己的现状（依据 [08-competitive.md](08-competitive.md)、[09-research.md](09-research.md)、[19-gap-analysis.md](19-gap-analysis.md)、[20-sessions.md](20-sessions.md)）：

- **已有**：哈希链 `journal.jsonl`（seq/verify/replay）、七类事件契约、技能按需注入、压缩、子代理委派 + manifest 漂移检测、预算（tokens/tool calls/steps）、inotify 观测式效果、lab bundle（manifest/inputs/artifacts/report/claims）；
- **没有**：记忆（gap #5，"结构性缺失"）、检索索引（"没有数据库、没有索引，会话 = 另一个 JSONL"）、会话列表。

对照下来，Qoder 真正值得抄的不是"向量 + 图谱"，而是**知识的产物化与生命周期**。按投入产出排：

### 6.1 值得抄的七件事

| # | Qoder 的做法 | 我们已有 | 我们缺 | 具体动作 | 成本 |
|---|---|---|---|---|---|
| K1 | 卡片带仓库/分支版本，随 commit 增量更新，切换分支时在"复用/增量/重建"间决策 | `manifest` 的 sha256、`producedBy{seq,tool}` 血缘 | **知识产物与代码版本的绑定** | 卡片 front-matter 强制 `source_sha` + `generated_at` + `generated_by_seq`；**独立的 `ui knowledge check`** 校验卡源是否仍一致（STALE 退 2），不塞进 `ui verify`——verify 要保持"只读日志、流式 O(1)" | 1d |
| K2 | 知识在任务前预取、任务中补查，且**用执行轨迹回放证明知识改变了结果** | 七类事件、`decision` 事件 | **"这次注入了哪张卡、为什么"** | 不新增契约大类：在 journal 里加 `knowledge/emit`、`knowledge/use` 两个子类型（带 card_id、命中原因、budget_tokens） | 1d |
| K3 | 人读（Wiki）与机读（Card）**两种表示同源** | compaction 摘要（人读）、`docs/` | **机读项目卡**：模块/入口/命令/约束/禁忌 | `docs/knowledge/*.md` + YAML front-matter；纯文件，零依赖，`sha256` 即索引 | 2d |
| K4 | 遗忘/去重/冲突检测是记忆系统的一等公民（打分驱逐，而非只追加） | 全量日志（磁盘保留）、内存 buffer 有上限 | **知识条目的退役与墓碑** | `knowledge/retire`（reason）+ 保留 tombstone；删除也留痕（比 Qoder 的"覆盖本地"强） | 0.5d |
| K5 | 团队共享走 Git（`.qoder/repowiki` 可提交、可评审），云同步只是可选 | 单二进制、无服务端依赖 | **知识的分发与评审路径** | 卡片随仓库提交，PR 里评审；不引入同步服务 | 0.5d |
| K6 | 增量触发：commit → 影响面 → 相关卡失效 | inotify 事件驱动、effect 观测 | **文件变更 → 卡片失效的映射** | 卡 front-matter 记 `touches: [glob]`；文件变更即标 `stale`，**不自动重写**（人决定） | 1d |
| K7 | 五条件对照 ×3 次 × CV 的实验设计 | lab bundle + `claims` + `lab table` | **知识 A/B 实验** | 用现成 lab 跑 with/without cards，`claims` 写提升值，报告带 CV | 1d |

### 6.2 只做一件事的话：K1 + K2

理由：**这是 Qoder 公开材料里最薄、而我们架构最厚的一处交叉点。**

Qoder 能生成知识、能检索知识，但它的公开材料里**没有任何"某次回答用了哪张卡、那张卡当时对应哪个 commit、谁确认过"的证据链**。我们有 seq + 哈希链 + 血缘 + claims，补上 `knowledge/emit` / `knowledge/use` 两个事件子类型，就能回答它答不了的问题：

```
这次 diff 为什么这么改？
  ← seq 41 knowledge/use card=mod-payment source_sha=9f2c… score=0.82 budget=1200
  ← seq 12 knowledge/emit card=mod-payment generated_by_seq=9 confirmed_by=human
  ← 卡源 9f2c… 与当前 HEAD 的 9f2c… 一致（verify 通过）
```

这条链一旦成立，"知识引擎"对我们就不是"更聪明的检索"，而是**可出庭的证据**——正好接上 [08-competitive.md](08-competitive.md) 里"事后说得清"的定位。

### 6.3 形态上可以学、实现上要换掉的三处

| Qoder | 我们该换成的 |
|---|---|
| 向量召回（云端 embedding，相似度不可解释） | **确定性优先**：卡片关键词/路径匹配 → `grep` 兜底；命中与未命中都入日志，可复现 |
| Memory Graph 多跳扩散（加权边，黑箱） | **显式 `related:` 链接**（人可读、可评审），不做自动图激活 |
| Agentic Search 独立子代理（多一层模型调用） | 我们已有工具面 + 委派机制；检索先做**过程内确定性检索**，确认不够再考虑子代理 |

---

## 七、不该抄的部分及理由

| 不该抄 | 具体是什么 | 为什么 |
|---|---|---|
| **N1 云端向量化** | 上传代码文件生成向量，只留向量（F8） | 与"零依赖 + 数据边界 + 可审计"三条同时冲突：数据出界、检索不可复现、多一个外部依赖。而且**向量本身不可验证**——我们的 `verify` 对它无能为力 |
| **N2 自动生成知识并自动当规范** | 扫描仓库直接产出 Spec/约定，Agent 直接遵从 | 会把"仓库里存在"误译成"组织认可"。第三方分析已经点破："下一代技术债不在代码本身，而在机器从旧代码里总结出的错误规范"（[觉醒AI](https://www.jxxy.net/ai/articles/qoder-research-engineering-knowledge-governance/)）。**必须有人确认位** |
| **N3 LLM 增强 commit message 后当史料** | 用模型补写低质量 commit message（F6） | 润色后的"意图"会让人分不清哪句是原始史料、哪句是模型解释（批评见[觉醒AI](https://www.jxxy.net/ai/articles/qoder-research-engineering-knowledge-governance/)）。若要做，**原文、生成时间、置信度、确认状态四样必须同时保留**，否则不可审计 |
| **N4 默认开启的采集与"云端覆盖本地"** | 客户端"同步团队知识"默认开，云版本覆盖本地（F12） | 未经确认的覆盖 = 不可审计的静默变更。我们的 `delegate` 都知道要检 manifest 漂移，知识更该如此 |
| **N5 记忆/知识"双飞轮"自调策略** | 元记忆监控并调整生成/组织/检索策略（F13） | 不可证伪、成本高、样本小；出了问题无法归因到具体策略变更。我们的预算与 effect 是**观测式**的，不该变成自修改 |
| **N6 用留存率/代码保留率当质量指标** | +11% 留存、+1.9% 保留率（F19/F16） | 代理指标，混淆因素极多（模型升级、UI 改动、用户结构）。它在 Qoder 内部有意义，对外不可比 |
| **N7 让主 Agent 自己翻记忆文件** | 官方已否定（"won't pause a coding task to scroll through memory files"） | 这条他们替我们试过了：文件接口对人类友好、对 Agent 不友好。我们的技能是**按需注入正文**，方向一致，别退回"让模型自己读目录" |
| **N8 "索引即产品"的规模假设** | 10 万文件、95% 仓库 1 分钟索引、Credits 计费生成 | 我们是单机、按日志、离线可复现；追求"索引大仓库"会直接压垮零依赖约束。**先服务 1 万文件以内的仓库** |

---

## 八、查不到 / 明确不猜

以下问题公开资料没有答案，**本文不做推测**：

1. embedding 模型、向量库、维度、chunk 大小与切分策略；
2. 知识图谱的存储格式（属性图？边权如何计算？版本演化边怎么落库）；
3. 检索排序权重、RRF 参数、reranker 具体型号；
4. IDE 侧知识注入的 token 预算与压缩策略（QoderWork 的 40K 不能外推）；
5. Memory 是否上传云端、保留期、跨设备同步实现（官方只写了"Cloud Sync 在路线图上"）；
6. 团队知识的**权限继承**：卡片是否继承源码 ACL？敏感业务含义如何脱敏？
7. 分支间知识冲突的仲裁规则（官方只提到"用 commit 历史/分支状态/改动范围选择复用/增量/重建"）；
8. 40 案例实验的原始任务集、评分 rubric、满分、脚本；
9. 知识卡的**准确率/幻觉率**——没有任何独立测量；
10. "越用越聪明"的量化定义。

第 6 条最值得警惕：一份第三方分析已经指出"源码没泄露，敏感业务含义可能已经泄露"（[觉醒AI](https://www.jxxy.net/ai/articles/qoder-research-engineering-knowledge-governance/)）。这是**观点**，不是对 Qoder 的事实指控，但它描述的风险类别是真实的。

---

## 九、参考链接

**官方文档**

- [Knowledge Overview](https://docs.qoder.com/user-guide/knowledge-engine/overview)
- [Repo Wiki](https://docs.qoder.com/user-guide/repo-wiki)
- [Knowledge Card](https://docs.qoder.com/user-guide/knowledge-engine/knowledge-cards)
- [Memory](https://docs.qoder.com/user-guide/knowledge-engine/memory)
- [Codebase indexing](https://docs.qoder.com/user-guide/indexing)
- [Context compression](https://docs.qoder.com/user-guide/chat/smart-context-control)
- [Enterprise Knowledge](https://docs.qoder.com/account/enterprise/knowledge-base)

**官方博客**

- [A Self-Iterating Knowledge Engine for AI-Native Software Engineering](https://blog.qoder.com/qoder-knowledge-engine)（2026-07-21）
- [Engineering Knowledge Engine: The Knowledge Layer Behind Harness Engineering](https://blog.qoder.com/engineering-knowledge-engine)（2026-03-12）
- [How we built a self-evolving memory system for Qoder](https://blog.qoder.com/qoder-memory-evolution)（2026-04-23）
- [Agentic Search: Agent-Driven Code Retrieval](https://blog.qoder.com/agentic-search)

**官方营销页 / 发布会**

- [Qoder 企业版（"越用越聪明"）](https://qoder.com.cn/enterprise)
- [极客公园：阿里发布 Qoder 1.0](https://www.geekpark.net/news/364252)（2026-05-15）
- [品玩转载](https://www.pingwest.com/w/313749)、[亿欧快讯](https://www.iyiou.com/briefing/202605151908087)、[站长之家](https://www.chinaz.com/ainews/28030.shtml)（同源新闻稿）

**第三方 / 社区**

- [Qoder 论坛：Repo Wiki 索引不全面](https://forum.qoder.com/t/repo-wiki-ai/2701/3)（2025-10-11）
- [腾讯云社区：QoderWork 技术深扒（记忆/会话/技能）](https://cloud.tencent.com.cn/developer/article/2707349)（2026-07-11，逆向，非 IDE）
- [觉醒AI：代码会过期，错误经验也会](https://www.jxxy.net/ai/articles/qoder-research-engineering-knowledge-governance/)（2026-07-17，评论）
- [CodexQA：40 道题跑 3 遍](https://codexqa.cn/en/articles/aliyun-qoder-knowledge-eval-zh)（2026-08-11，复述）
- [阿里云社区：我用 Qoder 写了三个月代码](https://developer.aliyun.com/article/1761760)（2026-09-09，用户自述，文中提到 "ContextDB"，未获其他来源确认）

---

## 附录 A：检索记录（可复现）

| # | 语言 | 主要 query | 用途 |
|---|---|---|---|
| 1 | 中/英 | Qoder Knowledge Engine 知识引擎 阿里 / repository graph memory / 发布会 | 找官方定义与发布会口径 |
| 2 | 中/英 | "Knowledge Engine" blog Harness Engineering / Repo Wiki 知识卡片 / 代码保留率 | 定位官方博客与数字来源 |
| 3 | 中/英 | docs.qoder.com knowledge / 使用文档 / 团队共享 | 找一手文档 |
| 4 | 中/英 | 实测 体验 评测 / honest review / Repo Wiki 缺点 | 找第三方证据 |
| 5 | 中/英 | HN discussion / vs Cursor / 记忆 云端 隐私 | 找独立评价与数据边界 |
| 6 | 中/英 | "越用越聪明" / 40% 11% 数据来源 / independent test | 核营销话术与复现 |

**未能完成的验证**：没有安装 Qoder、没有跑它的 40 案例、没有抓包看索引上传内容。凡涉及"实际效果"的结论，本文一律停在"厂商自述"。

---

## 九、落地情况（v0.1.15）

K1 + K2 已实现，但**按我们自己的标准打了几个折**，这里如实记：

| 项 | 计划 | 实际 | 说明 |
|---|---|---|---|
| K1 卡源校验 | `verify` 里增加 | **独立 `ui knowledge check`**（STALE 退 2） | `verify` 保持只读日志、流式 O(1) 内存；把文件哈希塞进去会破坏它的语义 |
| K1 空 `source_sha` | 应视为不一致 | **UNBOUND，默认退 0**，`--strict` 才红 | 否则仓库自带的示例卡会让 `check` 一直红；代价是"清空 sha"能绕过默认审计 |
| K2 知识注入点 | "任务前预取 + 任务中补查" | **只做任务前预取，且只在 `ui run`** | `chat` / `workflow` / `team` 的子任务不预取；"任务中补查"未做 |
| K2 命中排序 | 应该有相关性排序 | **不打分**（`score` 恒 `null`），超 3 张按 id 取 | 确定性优先：可复现胜过不可解释的相似度 |
| K3 卡片体积 | 未规定 | **无上限** | 超大卡会直接吃 token 预算；`budget_tokens = 正文/4` 语义保持干净 |

**没有做的事**（对应第八节的"不该抄"）：不自动生成卡片（N2）、不做 LLM 润色、不做云端。

**证据**：`ui knowledge check` / `knowledge/emit` / `knowledge/use` 各有集成测试；
`ui replay` 能看到 `knowledge/use` 行；`docs/knowledge/example-module.md` 是可跑通的示例卡。
