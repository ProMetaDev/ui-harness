# DSH 诊断：300 个插件之后，harness 缺的是一根脊梁

> 证据来源：`D:\deepseek_harness\resources\app.asar` 内的 `dsh/package.json` 与 305 个
> `@deepseek-ai/*` 包的 README/manifest（0.2.1-rc.5）。每个包都自带一节
> `Known Limitations and Deferred Work`，本文的结论基本都能在那里找到作者自述。

## 一、规模事实

| 指标 | 数值 |
|---|---|
| 一等方包（@deepseek-ai） | 305 |
| 其中 `dsh-client-ui-*` | 55 |
| 其中 `dsh-experimental-*` | 20 |
| 其中 `dsh-tool-*` | 34 |
| 其中 `dsh-api-*` | 12 |
| 会话相关包（session*/log/title/projection/query/format-v0→v4） | 24 |
| 会话格式版本 | 5 代（v0→v1→v2→v3→v4），仅支持相邻迁移 |

一个"读一个文件、改一个文件、跑一条命令"的循环，背后是 305 个包、一个自研 DI
框架（cordis）、一个 RPC 网关、55 个 UI 插件。这不是工程量大，是**概念没有收敛**。

## 二、六个系统性缺陷

### 1. 缝隙分裂（seamitis）

同一个能力被切成"接口 / 实现 / 策略 / 沙箱 / UI"五份，各自发包：

- 文件：`dsh-fs` → `dsh-fs-local` → `dsh-fs-sandbox` → `dsh-fs-observation-policy`
- 沙箱：`dsh-sandbox` → `dsh-sandbox-local` → `dsh-sandbox-policy` → `dsh-sandbox-windows-acl`
- 会话：`dsh-session` + `format` + `format-catalog` + 4 个迁移包 + `persistence` +
  `persistence-jsonl` + `projection` + `projection-cache` + `query` + `query-sqlite` +
  `reference` + `stats` + `telemetry` ×2 + `title` ×3 + `turn-outline` + `checkpoint-policy`
- 溢出：`dsh-spill` → `dsh-spill-local` → `dsh-spill-policy`

后果不是"解耦"，而是**行为变成几十个策略交互的涌现结果**：没有任何一个文件能回答
"这一步模型会看到什么、会被拦在哪、花多少钱"。端到端测试只能靠碰运气。

### 2. 模型可见的契约不是一等公民

- 系统提示由插件拼装，"没有终端用户编辑 API"（`dsh-system-prompt` 自述）；
- `toolOrder` 配错要到**第一轮**才炸，不是启动时；
- 工具描述里不得不写进沙箱怪癖：piped stdio 在受限子进程里 EPERM、
  read-only 下 pwsh 掉进 ConstrainedLanguage、FAT/NULL-DACL 例外……
- 运行时上下文把一个**不存在的路径**当成"实现 checkout"发给模型：
  `D:\...\app.asar\dsh\`（asar 是单文件归档，`app.asar\dsh` 不是目录）。

最能说明问题的是最后一条：harness 自己都不知道自己的真实位置，却要求模型"去查源码"。
模型看到的世界更像一份免责文档，而不是一份可操作的环境说明。

### 3. 权限语义太薄，体验与安全双输

`dsh-user-approval` 自述：只有一次性授权，没有 `allow-always`、没有规则库、没有撤销，
请求**不带工具参数**，没有 answerer 就 fail closed；`dsh-sandbox-policy` 自述：只管文件
效果，网络与进程策略不在词汇表里。

于是用户只有两个选择：盲批（看不到参数）或者卡死（没 UI 就永不通过）。模型那侧则是
反复烧轮次撞同一堵墙——README 里甚至记录了历史事故："十二个任务里五个零工具调用"。

### 4. 预算与上下文：全是近似，且近似不一致

- 分词器是 **4 字符 = 1 token** 的固定启发式，自述"低估 CJK 与 JSON Schema"；
- 循环**没有内置轮次预算**，靠扩展点自己取消；
- goal 只数轮次，不计量 token/钱/墙钟时间；
- 压缩失败时"带着超预算的完整历史继续跑"；
- `token-meter` 每次测量都克隆整个 surface（O(surface)），连阈值检查也不放过。

一个中文用户的 harness，按 4 字符/token 计价，等于系统性低估 2–3 倍。

### 5. 耐久性是事后补的

崩溃会丢掉"已被接受但未落盘"的 prompt；没有 WAL；子代理只有进程内驻留，跨进程需要
"durable mailbox + lease protocol"（自述为未来工作）；单写者锁、无回放、无幂等。

长任务（几小时的 goal / workflow）建立在一个"进程不许死"的假设上。

### 6. 自观测与工具调用韧性

harness 既不能向模型如实描述环境，也不能在自己的协议层出错时自救：

```text
DeepSeek Messages stream: tool input is invalid JSON
```

这一条是真实发生的：模型生成的 tool call 参数（一次较大的嵌套 JSON）被截断，
流式解析失败，DSH 直接**中止整步**——不修复、不重采样、不让模型看见自己写坏了什么、
不降级成部分可用调用。已经想好的那份内容和那次工具调用的上下文一起被丢掉。

工作区里 16 个 `pip-*` 临时目录（今天 11:33 生成）是另一个侧面的证据：子进程的
temp 语义没兜住，工具转而把临时文件写进了用户的工作区。

## 三、现象 → 根因 → 新 harness 的对策

| 现象 | 根因 | 对策 |
|---|---|---|
| 300 包，无人能预测一步的行为 | 概念未收敛，策略以插件形式无限增殖 | 单一进程、约 12 个模块、约 10 个工具；扩展点是**数据**不是服务 |
| 模型看到的世界不可复现 | 提示是运行时拼装的副作用 | 每步一份不可变 `PromptSnapshot`，可 diff、可回放、可计价 |
| 权限只有盲批/卡死 | 策略词汇表只有"文件模式" | 能力=效果集（读/写/网络/进程/密钥），规则可读可改，批准带参数预览 |
| 预算靠 4 字符估算 | 没有一等公民的预算对象 | 精确分词 + 硬预算（token/调用数/墙钟/费用），超支是错误不是静默 |
| 崩溃即失忆 | 副作用先做后记 | 先写 WAL 再执行，幂等键，resume 对账 |
| 协议错误整步报废 | 没有流式校验与修复回路 | 流式 JSON 校验 + 修复回合 + 参数大小上限 + 分块提交 |

## 四、一句话结论

DSH 不缺能力，缺的是**七条不可绕过的契约**：一步一快照、一调用一效果、一效果一决策、
一预算一账户、一副作用一日志、一委派一邮箱、一错误一修复。

新 harness 的价值不在"更多功能"，而在于让每一步都可解释、可回放、可计价、可中断。
