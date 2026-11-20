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
| 密钥 | 四层：`secrets/**` 无条件 deny ＋ 隔离副本不带该目录（`isolate::SKIP`）＋ 落账前脱敏门 ＋ `--private` 来源扣留（详见下面「隐私：两档处理 + 生命周期」） | 上下文里仍可能是明文（沙箱不限读）；日志与落盘那两份有档可查、可按保留期清掉 |

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
7. **策略的 deny 挡不住 shell 的读**：`secrets/**` 对 `fs_read` 有效，对 `cat` 无效（沙箱只限写）；`tools/kit.sh` 场景 5 至今仍是 GAP。

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


## OS 沙箱（Landlock，Linux 5.13+）

观测是**事后记账**，沙箱是**事前阻止**。Linux 上用 Landlock 给 shell 上锁，
零依赖直接调 syscall（不用 libc crate、不用 bwrap、不需要 root）：

| 路径 | 权限 |
|---|---|
| 整个文件系统 | 读 + 执行（否则 exec 不了 /bin/sh） |
| 工作区 | 全部 |
| `/tmp` | 全部 |
| `/dev/null` | 读写（重定向不报错） |

**fail closed**：要沙箱却拿不到（内核不支持），命令**直接失败**，不会静默降级成
无沙箱执行。`UI_SANDBOX=0` 是显式关闭，日志 meta 里会记 `sandbox:"off"`。

每条 shell 调用的结果里都带沙箱标记：

```json
{"callId":"call-1","tool":"shell","ok":true,"meta":{"sandbox":"landlock:7","envScrubbed":0}}
```

**边界**：

| 项 | 说明 |
|---|---|
| 网络 | 默认**不**限制；`UI_SANDBOX_NET=1` 时禁 TCP（ABI ≥ 4）。UDP 不受 Landlock 限制。 |
| Windows | **没有对等机制**，shell 仍是"可执行程序"级信任。 |
| 读 | 系统文件可读（不然跑不了程序），所以 `cat secrets` 这类**读**仍然拦不住。读回来的正文随后走隐私两档，见下一节。 |
| 环境 | 靠 `envScrubbed`（另一条防线），不是 Landlock。 |

## 隐私：两档处理 + 生命周期

日志的价值是"能复盘"，代价是"它把你的密钥也存了一份"。所以正文在**落账前**分两档处理，
两档问的问题不一样，挡住的也不是同一件事。

### 第一档：落账前脱敏（默认开）

`Journal::append` 是唯一入口，先过 `secrets::redact_json` 再写盘。命中密钥形状的
（`api_key = "sk-…"`、`Bearer …`、PEM 头、`X-*-Token` 之类）换成 `[REDACTED:<label>]`，
并在同一条事件上盖 `redacted: N`——**N 是"这一条抹了几处"**，它让人知道要回工作区找原文，
而不是让"日志里没有"这句话变成假话。

绕过这道门必须显式写 `append_raw`。全内核只有 3 处，每处都有注释说明为什么：

| 位置 | 写什么 | 为什么允许绕 |
|---|---|---|
| `agent.rs:824` | `tool/output` 的正文 | 正文已经先过了下面两档；再过一遍门会把 `redacted` 计数的含义洗掉 |
| `agent.rs:478` | 落盘原文的指针记号 | 记号是给原文作证的，里面只有路径、字数与指纹 |
| `memory.rs:325` | `memory/write` | 记忆条目是原文的另一个藏身处——所以 `ui secrets` 单独扫 `.ui/memory.jsonl` |

关掉它：`--no-redact` 或 `UI_REDACT=0`。**关这件事本身会写一条 `privacy/redact-off`**，
日志里能看出"这一段没脱敏"，而不是静默少一层。

### 第二档：来源扣留（`--private` / 命中隐私清单）

脱敏是模式匹配，它不知道一串随机字符是不是密钥。第二档不问内容，问**来源**：
工具效果的 target 落在隐私清单里（默认那几条，加上你用 `--private PATH` 追加的），
整段正文就不进上下文、也不进日志——

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

- **模型上下文里仍是明文**。沙箱不限读，`cat secrets/x` 的内容照旧进上下文；
  第二档只在来源命中清单时拦，第一档只管日志。
- **第一档是模式匹配，不是 DLP**。没有密钥形状的机密照原样进日志。想知道有没有漏：
  `ui secrets`（扫 journal、两个落盘目录与 `.ui/memory.jsonl`）。
- **网页端已过这两档**（2026-11-20）：`Hub::push` 与 `store_tool` 是仅有的两个出口，
  都过同一个 `redact_json`；`/snapshot` 因此不会再嵌进明文。
  注意这仍**不是 DLP**：没有密钥形状的机密照旧看得见（和日志同一档判据）。
- **同机 root、整个工作区打包外发**不在防护范围内。
- **读隐私路径不拦**（Phase 1-B）：靠工具层与策略，不靠沙箱。

## 日志写不进去时（fail closed）

审计的价值在于「记录一定存在」。所以日志不可写时，agent **立刻停止**，
不会在没有记录的情况下继续执行命令：

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
