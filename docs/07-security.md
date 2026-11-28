# 安全边界

威胁模型很简单：**模型是不可信的**。它可能被提示注入、可能自己犯错、可能被用户之外的输入影响。
所以所有约束都不能靠"提示词里写了"来成立，必须由内核强制。

## 边界一览

| 边界 | 谁在管 | 失效会怎样 |
|---|---|---|
| 效果策略 | `policy.rs`：每条效果先算决策（allow/ask/deny），`secrets/**` 无条件拒绝 | 越权读写、执行任意命令 |
| 审批精确性 | 会话规则是**字面量匹配**（`Rule.literal`） | 批准 `echo *` 后 `echo pwned` 免审 |
| 路径折叠 | `Ctx::resolve` 做词法折叠，`..` 不能离开工作区 | `fs_read ../secret.txt` 读到外面 |
| 真实路径 | `Agent::escapes_workspace`：`canonicalize` 后必须仍在工作区内 | 符号链接把工作区外的内容读进来 |
| 读取例外 | `protected::is_allowed_read`：字面前缀 **＋** 真实路径都要落在 `<ws>/.ui/tool-output/`，且只对普通文件生效 | `.ui/tool-output/link -> secrets/x` 这种软链借到"截断缓存"的读例外 |
| 敏感单文件 | `protected::is_sensitive_file_name`：`.env*`、`id_rsa`、`*.pem`、`*.key` 等；`fs_read` / `grep` / `fs_search` / `tree` 与写入口 `fs_write` / `fs_edit` / `patch` / `fs_mv` 共用，并按真实路径识别软链别名 | `.env` / 私钥被读出来、搜出来、列出来或改掉 |
| 密钥 | 四层：`secrets/**` 无条件 deny ＋ 隔离副本不带该目录（`isolate::SKIP`）＋ 落账前脱敏门 ＋ `--private` 来源扣留 ＋ Linux 读侧路径遮罩（详见下面「隐私：两档处理 + 生命周期」） | Windows/关闭读沙箱时上下文仍可能是明文；日志与落盘那两份有档可查、可按保留期清掉 |

## 已完成的安全修复（来自对抗测试）

| 问题 | 触发 | 修复 |
|---|---|---|
| `..` 路径穿越 | `fs_read ../outside.txt` 被 `<ws>/**` 放行 | 词法折叠后再判 |
| 符号链接逃逸 | `link.txt -> ../secret.txt` | 真实路径必须落在工作区内 |
| 审批授权放大 | 批准 `echo *` / `a?b` 后同类目标免审 | 会话规则改精确匹配 |
| JSON 深嵌套栈溢出 | 20 万层 `[` | 深度上限 128 |
| 数字失真 | `1e400` → inf → 编码成 `null` | 拒绝非有限数 |
| 预算被清零 | provider 回报 `usage=u64::MAX` | i128 + 饱和 |
| 预算静默失效 | `--max-tokens 0` 被当成无限 | 0/负数 = 一分不许花（fail closed） |
| 搜索 DoS | 24 个 `*a` 的 pattern 指数回溯 | 贪心 + 回溯点，O(n·m) |
| 日志丢记录 | 尾记录缺换行 → 下次 append 粘连 → 再下次静默截断 | append 一次写完；open 补换行 |
| 日志打不开 | 撕裂切断 UTF-8 多字节字符 | 按字节读、逐行解码 |
| panic 类 | `fs_read` limit 溢出、`keep_recent` 溢出 | saturating / clamp |
| 超时被绕过 | `sleep 3 & wait` + `timeoutMs:200` 实测 3010ms | 命令自成进程组，超时/取消时 `kill(-pgid)`；实测 3010ms → 247ms |
| 内存被撑爆 | `fs_read` 读 1GB 文件整个进内存 | 超过 8MB 直接报错，要求 `offset/limit` 分页 |
| 并发写断链 | 两个进程各持旧 seq 追加 → 链断、日志不可用；`open` 的「不存在就写空串」还会截断别人的记录 | 写前 `flock` 独占锁 + 补读别人追加的记录 + 原子创建（`create(true).append(true)`）；事件带 `w`（writer id），`verify` 按 writer 分轮次 |

## 还没解决（明确记录）

1. **`ui serve` 只有单用户 token 鉴权**：默认只绑 `127.0.0.1`，访问页面/API 需要 `?t=<token>`（用后种 Cookie），`--token off` 可显式关闭；仍不要直接暴露到局域网或多用户环境。
2. **策略的 `?` 会匹配 `/`**：文档只说"一个字符"，实现更宽；已记录未改（改了会影响现有规则）。
3. **Windows 上的进程树与文件锁**：`kill_tree` 走 `taskkill /T /F`，锁走锁文件兜底（60s 过期），都只在 WSL 里验过 Linux 路径。
4. **多 writer 的 turn 归属只到 writer 级**：同一个 writer 内部仍是串行的，跨进程的因果顺序无法从日志推出来（只有全局 seq 定序）。
5. **网页端已脱敏（2026-11-20 修）**：`Hub::push` 与 `store_tool` 两处都过 `secrets::redact_json`，事件带 `redacted: N` 标出抹过几处。判据与 `Journal` 落账门共用一份，所以两边不会漂。回归由 `tools/verify_web_leak.sh`（门禁）与 `server::tests::web_surface_redacts_before_it_leaves_the_process` 守着。~~此前是原文~~：`/snapshot` 会把整份 `hub.log` 嵌进**可分享**的 HTML，那一版等于转发文件就公开工具输出。
6. **脱敏门只管日志、只管模式匹配**：无密钥形状的机密会照原样进日志，上下文里更是明文。它是"日志别变成密钥库"，不是 DLP。
7. **Windows 没有对等读侧视图**：Linux 默认遮住 `secrets/**` 等路径；Windows 或显式关闭读沙箱时，`cat` 仍可能读到内容，只能靠 `read.private` 审批、来源扣留与落账脱敏收敛。
8. **硬链接同 inode 别名不在路径防护范围**：路径防护（`..` 词法折叠、`escapes_workspace` / `is_allowed_read` 的 `canonicalize` 真实路径、`is_private_path` 与 `is_sensitive_file_name` 的清单）都按**路径**判，不按 inode。`ln` / `CreateHardLink` 造出的别名与目标共享 inode，但路径上没有 symlink 跳转，`canonicalize` 也不会把别名变成目标路径；因此工作区内的硬链接、以及从工作区外链进工作区的硬链接（同 inode）都能绕过按路径写的隐私/敏感判断。要识别它必须比较 inode/文件 ID 并扫描目录；本轮不做全盘 inode 扫描，也不声称能按路径识别硬链接。这是已接受的残余风险，不是"已覆盖"。
9. **审批执行与 resume 的重复脱敏消费仍是既有 P2**：审批卡本身已改用 `tool/request` 的 Journal 安全副本，但 `tool/start` 仍会把同一份 args 再送入脱敏流；当前回归覆盖到拒绝审批，未覆盖批准后执行及 Hub PEM 审批卡，resume 使用脱敏参数重放也可能改变语义。这里不声称审批执行链已完整覆盖。

## 怎么复查

```bash
bash tools/test.sh                  # 全量测试（数量以脚本输出为准）
bash tools/verify_escape.sh         # 路径穿越的 E2E 复现（应被拒绝）
bash tools/verify_pending.sh        # 退出码 + 符号链接（应被拒绝）
bash tools/verify_shell_timeout.sh  # 超时必须连后台进程一起收掉（应 < 300ms）
bash tools/verify_concurrent.sh     # 3 个进程并发写同一份日志（链必须完整）
bash tools/verify_privacy.sh        # 脱敏门 + 两目录 + 保留期（结论数以输出为准）
bash tools/verify_docs.sh           # 文档里的隐私/沙箱说法与代码是否还一致
```


## 信任边界（必须说清楚的一条）

策略仲裁的是**"要不要执行某个动作"**，不是"这个动作内部做了什么"。

| 通道 | 我们仲裁什么 | 我们**不**仲裁什么 |
|---|---|---|
| `fs_*` / `grep` / `tree` | 每个具体路径 | —— |
| `shell` | 这条命令要不要跑 | 命令内部的读写、联网、读密钥 |
| `plugin_call` | 声明的那几条 effects | 声明之外的任何行为 |
| `mcp_call` | server/tool 这一层 | 外部 server 进程内的行为 |

所以：**shell / 插件 / MCP 是"可执行程序"级别的信任，不是沙箱内的受限代码。**
`ui doctor` 会把这条直接打在输出里，不让它藏在文档里。

这条来自一次外部评审的质疑（"顶层 proc.exec 获批，并不证明内部的
secret.read/net.fetch 被仲裁"）——它说得对，所以我们现在明确标注，
而不是暗示"效果集覆盖一切"。

## 观测式效果（对上面那条的部分补救）

声明看不到内部行为，那就**观测**：`shell` / `plugin_call` / `mcp_call` 执行前后
各扫一次工作区（只读元数据：大小 + mtime），把实际改动写成 `effect/observed`：

```json
{"kind":"effect/observed","callId":"c1","tool":"shell","summary":"+2 ~0 -0",
 "changes":{"added":["out/a.txt","out/b.txt"],"changed":[],"removed":[],
            "scanned":37,"truncated":false}}
```

`ui replay` 会显示成一行：`shell · 实际改动 +2 ~0 -0 · 例 out/a.txt`。

**边界要说清楚**：

| 说清楚 | 说明 |
|---|---|
| 只看工作区内 | 命令写到工作区外面，观测不到 |
| 只看文件元数据 | 内容级别改动（同大小同 mtime）可能漏掉 |
| 跳过重目录 | `.git` / `target` / `node_modules` / `.ui` 等不看 |
| 超过 2 万文件截断 | 标记 `truncated:true`，不假装完整 |
| 约 11ms / 2000 文件 | 大仓库可以 `UI_OBSERVE=0` 关掉，代价是审计看不到实际改动 |

它补的是"**说了要执行，但没说做了什么**"这个缺口；它**不能**替代 OS 沙箱——
观测是事后记账，沙箱是事前阻止。


## OS 沙箱（Linux 读侧最小根 + Landlock）

观测是**事后记账**，沙箱是**事前阻止**。Linux 默认先建立读侧最小根，再用
Landlock 限制写入；零依赖直接调 syscall（不用 libc crate、不用 bwrap、不需要 root）：

| 层 | 机制 | 边界 |
|---|---|---|
| 读侧视图 | user/mount namespace + chroot | 只绑定系统运行时、工作区、`extra_writable` 与显式 `read_roots` |
| 隐私遮罩 | 空目录/空文件 bind | 工作区隐私/敏感路径与已声明的绝对隐私路径不可读 |
| 逃逸防护 | 清 capability + seccomp | 禁止重新挂载/卸载、`chroot`、`pivot_root`、`setns`/`unshare` 等 |
| 写侧 | Landlock | 只允许工作区、`extra_writable` 与私有 `/tmp` 写入 |
| 网络 | Landlock ABI ≥ 4 | 默认允许；`UI_SANDBOX_NET=1` 禁 TCP，UDP 不受限 |

读侧视图不挂 procfs，避免 `/proc/self/root` 等 magic link 绕过 chroot；`/tmp` 是
独立空 tmpfs，`/dev` 只暴露 null/zero/random/urandom/tty。系统根以只读 recursive
bind 进入视图，merged-/usr 保留 `/bin`、`/sbin`、`/lib*` 兼容软链。绝对路径启动的
外部程序及其 MCP 脚本所在目录会自动加入只读白名单，避免解释器打不开脚本。
插件配置可以放在 `.ui`，但可执行入口不要放进 `.ui` 或 `secrets` 等受保护路径；
否则读侧视图会按同一隐私规则把它遮住，插件应使用普通工作区路径或显式白名单。

**fail closed**：要沙箱却拿不到 user/mount namespace、Landlock 或挂载能力，命令
**直接失败**，不会静默降级成无沙箱执行。`UI_SANDBOX=0` 是显式关闭；Linux 还可设
`UI_SANDBOX_READ=0` 只关读侧视图，日志 meta 会记 `sandbox:"off"`。

每条 shell 调用的结果里都带沙箱标记：

```json
{"callId":"call-1","tool":"shell","ok":true,"meta":{"sandbox":"landlock:7","envScrubbed":0}}
```

**边界**：

| 项 | 说明 |
|---|---|
| 网络 | 默认**不**限制；`UI_SANDBOX_NET=1` 时禁 TCP（ABI ≥ 4）。UDP 不受 Landlock 限制。 |
| Windows | **没有对等机制**，shell 仍是"可执行程序"级信任。 |
| 读 | Linux 默认遮住隐私/敏感路径；命令串命中时仍追加 `read.private`（默认 ask）并留审批记录。`effect/observed` 的 `provesRead:false` 表示观测不证明实际读取，不否定 OS 路径遮罩。 |
| 硬链接 | 同 inode 的硬链接别名不在路径遮罩范围；见 SECURITY 的明确边界。 |
| 环境 | 靠 `envScrubbed`（另一条防线），不是沙箱。 |

## 隐私：两档处理 + 生命周期

日志的价值是"能复盘"，代价是"它把你的密钥也存了一份"。所以正文在**落账前**分两档处理，
两档问的问题不一样，挡住的也不是同一件事。

### 第一档：落账前脱敏（默认开）

`Journal::append` 是普通事件的唯一入口，先过 `secrets::StreamRedactor::redact_json` 再写盘；`tool/output`
走 `append_with_streamed_text`，让原始正文在根字段原位置进入同一道跨事件门并累加 `redacted` 计数。
来源扣留时仍推进状态但不发布正文，因此截断的 PEM 头也能抑制后续输出中的正文。
所有字符串都做静态凭据检测，正文类字段额外做跨事件前缀保护；只有事件根层的既有结构字段只做静态
检测，嵌套的 `args` / `effects` 等模型可控字段不能按 `path`、`name`、`url` 等键名豁免；根层
`path` / `file` / `filename` / `artifact` 以及观测里的 `added` / `removed` 等模型可控名称同样不能豁免。
`prompt/snapshot` 的 `snapshotId` 和 `sections` 元数据也按内核结构做静态检测：它们是 16 位哈希或
固定段落 id，不是正文。若让随机哈希进入跨事件流，它可能与已发布的公开前缀拼成完整凭据形状，再被
下一段元数据补全并清空状态，随后真正的正文尾部就会以明文发布；同名 `sections` 出现在模型可控的
`args` 里时仍必须走跨事件门，不能按名字豁免。
命中密钥形状的
（`api_key = "sk-…"`、`Bearer`/`Basic` 认证头、PEM/SSH2/PGP 私钥头、`X-*-Token` 之类）换成 `[REDACTED:<label>]`，
并在同一条事件上盖 `redacted: N`。N 是来源处理与本次发生变化的字符串字段计数之和，不保证等于秘密的
精确出现处数；它表示这条记录确实被处理过，原文仍需回工作区找。

网页端还有一条容易漏的路径：模型 delta 可能把 `AKIA` 和后半段拆成两个事件，甚至跨空回复或跨任务
拆分；逐事件脱敏与“合并后再发”都无法覆盖任意边界。因此 `WebHooks` **不缓存、不发布模型增量原文**，
只在当前回复的首个增量处发固定安全提示，事件边界重置。Journal 与 Hub 的跨事件状态跨 JSON 字符串、
普通事件和任务持续，**不在每个字符串或 assistant 回调结束时 `finish`**；已发布的裸前缀候选跨中性事件
保留，PEM 头/正文/尾在匹配 END 前一律不发布。未闭合正文缓存上限 4096 字节；前缀候选去重后最多
512 条 / 16 KiB，超限即封锁当前威胁边界，直到显式 `reset` 或 `finish`，不会静默驱逐安全候选。
显式 `reset` 只用于放弃整个威胁边界；正常同进程切会话保存/恢复状态，不调用 `reset`。需要主动
关闭一段独立文本流时才显式 `finish`。已进入 pending 的未闭合认证头若被短值和终止符断开，
按 `[REDACTED:partial]` 扣留其头部，终止符后的中性正文仍正常发布，不能原样清掉状态后
在下一字段继续携带 token 尾部。

Journal 的每条记录都带内核生成的 `redactionState`：只保存 `clear`、`suppress`、`sealed`、PEM 结束标记编号，以及经 `is_bare_partial_prefix` 验证的公开凭据头片段；**绝不把 pending 或 token 正文写进状态**。未闭合 pending 跨进程统一降级为 `suppress`，PEM 则保留对应的 END 匹配状态。脱敏已经移到 Journal 写锁内，顺序固定为“补读并恢复上一条状态 → 脱敏当前事件 → 写正文与状态”，因此多 writer 按实际落盘顺序共享威胁边界，不会各自在锁外先过一道门。重开进程、同进程重开 Journal 都会从最后一条记录恢复；状态与正文属于同一条哈希链记录，不存在正文已写而状态没写的崩溃窗口。`open` 验证尾部记录的 `prev`/`hash`，`catch_up` 验证每条补读记录的 `prev`/`hash`；记录被篡改时拒绝续写，不能只凭 JSON 可解析就接写。

旧版日志没有 `redactionState`，无法证明历史里是否留下过未闭合候选，因此 `Journal::open` 对非空旧日志 **fail closed**：只能 `Journal::read` 只读查看，不能猜测成 `clear` 后续写；需要新建会话日志。Hub 重启或首次切换到某个会话时也从 Journal 恢复安全状态，再静态重放已经落账的历史，重放不会消耗活动流。同进程切会话按日志真实路径保存/恢复完整状态，但快照同时绑定 Journal 的 `seq`/`hash`；若其他进程在离开期间追加过记录，旧快照作废，改用最新持久状态。`/api/ready` 与 `/api/meta` 只做静态 `redact_json`。短凭据、短键名值和部分厂商前缀仍有长度阈值，模式匹配不是完整 DLP。

Hub 的 assistant、user、工具摘要、工具全文与 replay 共用同一条跨事件状态链，秘密不能拆在两个外发出口之间还原；同一工具分片只推进一次，摘要从安全全文派生，模型可控的工具名也与正文共用这条流。`/api/tool/{id}` 与 SSE/backlog/可分享 `/snapshot` 读到的都是已经过门的输出。来源命中隐私清单时，Agent 在调用展示 Hook 前就把正文与错误替换为扣留提示，Hub 收不到 private 原文。用户输入、assistant 正文和工具全文都先写 Journal，再把同一条安全事件中的正文交给展示 Hook；Hub 不会拿到模型/工具原文，因此进程在“落账”和“发布”之间崩溃也不会在重启后拼接出秘密。审批卡参数复用 `tool/request` 的 Journal 安全副本，其他展示字段取自 `approval/requested` 的安全副本；原始 args 不会在审批事件里重复落账，政策决策提示也取自 `tool/request` 的安全字段，不能把原始参数直接送入 Hub。会话切换按日志真实路径保存并恢复 Hub 状态；状态保存、当前会话替换与历史重放在同一把 Hub 发布锁下完成，并发事件不能插进半切换状态。恢复后的历史重放只做静态展示，不消耗当前 PEM/候选状态；重启与首次切换从 Journal 的 `redactionState` 恢复，不从空状态开始。会话名本身也是外发字段：CLI 与 Web 都拒绝凭据形状名字，列表跳过磁盘上已存在的此类文件名。
会话切换采用两阶段：目标日志、历史和 Hub 脱敏流全部准备成功后才提交；恢复失败时 Agent 与 Hub 都保留原会话，不会把旧 Hub 流绑定到切换后日志的 `seq`/`hash`。失败工具的 stdout 仍进入模型历史、Journal 与 Web 全文，但 TUI/Console/Web 摘要优先展示从 Journal 已脱敏正文派生的错误，不能直接把原始 `error` 绕过脱敏门发布。

代价是网页不再显示逐字流式正文，这是 0.3.0 为消除可拼接明文分片主动做出的安全取舍。

绕过静态检测这道门必须显式写 `append_raw`。全内核只有 2 处明确例外，每处都有注释说明为什么：

| 位置 | 写什么 | 为什么允许绕 |
|---|---|---|
| `agent.rs::note_redact_off` | `privacy/redact-off` 记号 | 固定原因字符串，用来显式标记关闭脱敏的区段 |
| `memory.rs:325` | `memory/write` | 记忆条目是原文的另一个藏身处——所以 `ui secrets` 单独扫 `.ui/memory.jsonl` |

关掉它：`--no-redact` 或 `UI_REDACT=0`。**关这件事本身会写一条 `privacy/redact-off`**，
日志里能看出"这一段没脱敏"，而不是静默少一层。

### 第二档：来源扣留（`--private` / 命中隐私清单）

脱敏是模式匹配，它不知道一串随机字符是不是密钥。第二档不问内容，问**来源**：
工具效果的 target 落在隐私清单里（默认那几条，加上你用 `--private PATH` 追加的），
整段正文就不进日志和网页出口；模型上下文仍按这次已批准的读取保留原文（长输出只留首尾与指针）——

```json
{"kind":"tool/output","callId":"call-3","tool":"shell","ok":true,"text":"",
 "withheld":"secrets/**","chars":8192,"sha256":"9f2c…",
 "file":".ui/tool-private/0007-a1b2c3d4-9-shell.txt"}
```

`withheld` 的值就是**命中的那一条清单**，所以账上还能回答"按哪一条扣的"。

字数与指纹仍在，所以"这一步看了多少、看的是不是同一份"依然可复盘；正文只在磁盘上。
清单只有命令行能加（`--private` 可重复、可 `a;b`，或 `UI_PRIVATE="a;b"`），
`ui doctor` 会把此刻生效的条目连"默认/声明"来源一起打出来——不然人加了 `--private`
却没有地方能确认它真的登记上了。

### 两个落盘目录，权限故意不一样

| 目录 | 里面是什么 | 模型能不能读 |
|---|---|---|
| `.ui/tool-output/` | 被截断的长输出全文（按需展开的缓存） | **能**——`.ui` 里唯一的读取例外，且不豁免写 |
| `.ui/tool-private/` | 按清单扣留的原文 | **不能**——刻意不在任何读取例外里 |

例外只对**真实路径位于 `<ws>/.ui/tool-output/` 里的普通文件**生效：
`is_allowed_read` 先判字面前缀（相对工作区必须写在 `.ui/tool-output/` 下），
再 `canonicalize` 真实路径并要求它仍严格落在这个目录里。所以
`.ui/tool-output/link -> secrets/astra.json`、指向工作区外的软链、target 不
存在的悬空链接一律不算例外（悬空链接 fail closed）；`.env*`、`id_rsa`、
`*.pem`、`*.key` 这些敏感文件名也没有例外档。`ui serve` 的 `/api/file` 不
继承这条例外（`.ui` 一律挡住），对敏感名另外返回 403；`/api/tree` 不列出
这些名字。

敏感名不是只挂在 `fs_read` 上：`grep`（含没有系统 grep 时的 `literal_search`）、
`fs_search`、`tree` 在遍历和返回结果时按同一份清单过滤，`grep` 还会用
`--exclude` 在读取前粗筛；写入口 `fs_write` / `fs_edit` / `patch` / `fs_mv`
对这些名字一律拒绝。判据看两处——调用方给的名字，和 `canonicalize` 之后的
真实名字——所以 `notes.txt -> .env` 这种软链别名在读写两侧都躲不掉。
这里故意不给"编辑配置文件"开例外：能改 `.env` 就等于能把泄漏的密钥写回或
换掉，代价比少编辑一个文件大；配置要换文件名请直接改名。

这个不对称就是第二档的全部意义。如果扣留的原文也放进可读目录，那日志里
"全文在 .ui/tool-output/xxx.txt"那句指针本身就是给模型看的提示词：
它只要 `fs_read` 一下，第二档就白做了。同理，被截断的普通输出**不能**因为
"顺便"就落进扣留目录——那是给人看的，模型按需展开要读不到东西。
文件建出来 0600、目录 0700；Windows 无此语义，原样创建。

### 生命周期：原文不会一直攒着

两个目录合起来算**一份**预算：默认 7 天 / 200 个 / 256 MiB，超了从最旧的删。
"最旧"按 `max(mtime, atime)` 判——刚被展开读过的文件不会因为"很久以前写的"而被清。
拆成两个目录之后这件事必须合并算：每个目录各领一份预算，等于 200 个变成 400 个。

| 命令 / 开关 | 作用 |
|---|---|
| `ui spill` | 看脚印：几个文件、多少字节、下一次会清掉哪些（`--json` 给机器） |
| `ui spill clean [--dry-run]` | 按当前档位清；`--all` 一个不留，且**必须**配 `--yes` |
| `UI_SPILL_MAX_DAYS` / `UI_SPILL_MAX_FILES` / `UI_SPILL_MAX_BYTES` | 三档上限（尺寸那档支持 `512b/1k/256m/2g`） |
| `UI_SPILL_KEEP=all` | 全留档；`ui doctor` 会 ⚠ 提醒你现在在这档 |

四个要说清的细节：**只认内核自己的命名**，认不出的文件不碰，`foreign` 计数告诉你有几个；
knob **写错会打 ⚠ 并给出正确写法**（`UI_SPILL_KEEP=0` 会提示你"一个不留"该写
`UI_SPILL_MAX_DAYS=0`），不会静默退化成"全留"；**run 边界自动清一次**（60 秒内不重复），
清完写 `privacy/spill-pruned`、失败写 `privacy/spill-error`（只记目录名，不记绝对路径）；
**目录列不出来时 `ui spill`/`doctor` 说"数字不可信"**，而不是报 0 个。

清掉之后日志里那句指针会指向不存在的文件——这是正常结果，但必须有人说：
`ui doctor` 有一行「日志里有 N 条指针指向已过保留期的原文（字数与指纹仍可核对，
正文找不回来了）」，`fs_read` 那个路径时回答的是"已按保留策略清理"，不是"文件不存在"。

### 这两档没有挡住什么

- **模型上下文里仍可能是明文**。Linux 读侧视图阻断清单内路径，但硬链接别名、
  非清单机密、Windows 或显式关闭读沙箱的执行仍可能把内容带进上下文；命令串命中时
  会触发 `read.private` 审批并留下 `provesRead:false` 的 observed 记录。第二档只在
  来源命中清单时拦日志与上下文。
- **第一档是模式匹配，不是 DLP**。没有密钥形状的机密照原样进日志。想知道有没有漏：
  `ui secrets`（扫 journal、两个落盘目录与 `.ui/memory.jsonl`）。
- **网页端已过这两档**（2026-11-20）：`Hub::push` 与 `store_tool` 是仅有的两个出口，
  都过同一个 `redact_json`；`/snapshot` 因此不会再嵌进明文。
  注意这仍**不是 DLP**：没有密钥形状的机密照旧看得见（和日志同一档判据）。
- **同机 root、整个工作区打包外发**不在防护范围内。
- **读侧视图不是全盘 inode 防护**。Linux 按路径遮罩，硬链接别名仍可读同一 inode；
  Windows 没有对等读侧视图。命令串检测也不是 shell 解析器，变量展开、拼接或脚本
  间接读取可能漏判，因此 `read.private` 不能替代 OS 边界。

## 日志写不进去时（fail closed）

审计的价值在于「记录一定存在」。所以日志不可写时，agent **立刻停止**，
不会在没有记录的情况下继续执行命令；Web worker 补记排队取消时如果切会话或写账失败，
也会广播错误并退出 worker，不会吞掉错误继续跑后面的任务：

```console
$ ui run ...
ui: 打不开日志（/tmp/ws/.ui/journal.jsonl）：Permission denied (os error 13)。
    日志是执行记录，写不进去就不该继续执行；请检查磁盘空间与文件权限后重试。
```

三个已验证的场景（`tools/verify_failclosed.sh`，已进 `tools/debug.sh` 门禁）：

| 场景 | 工具是否执行 | 退出码 |
|---|---|---|
| 日志文件只读 | 没有执行 | 1 |
| `.ui` 被普通文件占住（日志建不出来） | 没有执行 | 1 |
| 修好权限后 | 正常执行 | 0 |

这条不能靠"应该没问题"——它属于「一旦破了，整个审计承诺作废」的那类。
