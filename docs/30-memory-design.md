# 30 · 知识与记忆：事件派生、可验证、可删除的跨会话记忆

> 状态：设计稿（2026-11-17），尚未实现。
> 依据：`ui-core/src/journal.rs`、`agent.rs`、`budget.rs`、`observe.rs`、`isolate.rs`、
> `ui-cli/src/main.rs`，以及 `docs/02-design.md`、`docs/20-sessions.md`、`docs/23-status.md`。
> 一句话：**记忆不是第二套真相，而是从日志里提炼出来的、可回指到具体 seq/hash 的少量事实。**

## 0. 摘要

- 存储复用 `Journal`：`<ws>/.ui/memory.jsonl`，同样是 append-only JSONL + SHA-256 链，
  自动获得 `verify`、`replay`、多进程 `flock`、尾部撕裂修复，不引入数据库。
- 写入只有三条路：显式 `ui remember`；从 `model/repair` 提炼；从
  `delegate/end + team/accept` 提炼。后两条**只产生候选**，`memory accept` 之后才生效。
- 读取只在每轮开始时做一次：零依赖 BM25 词频检索 + 路径/类型加成，top-k 进快照的
  `memory` 段（可计价），同时写一条 `memory/used` 说明"这一轮用了哪几条"。
- 淘汰靠 `expiresAt`、`ui forget` 墓碑、非空 `key` 的 `(scope,key)` 唯一约束；引用关系由
  `ui verify` 逐条核对，坏链/悬空引用/伪造来源一律退出码 2。
- 不做 embedding、不做代码库索引、不做云端、不让模型直接写记忆、不让命中次数自动提高置信度。

## 1. 问题定义：为什么"用完就忘"

### 1.1 现有机制的生命周期

| 机制 | 活多久 | 丢失方式 |
|---|---|---|
| `Agent.history: Vec<Message>` | 一个 `Agent` 进程内 | 进程退出即丢；`switch_journal` 按新日志重建历史，旧会话内容清空（`docs/20`） |
| `compact` | 超阈值就折叠 | 只保留条数、工具统计、被截断的用户问题；具体命令、错误原文、偏好会丢（`compact.rs`） |
| `skill` | 文件在就在 | 静态内容，人工维护；不会从一次修复里长出来 |
| `delegate/end` | 父日志永久、父上下文一轮 | 摘要只作为一条 `note` 进入父 history，之后没人再读；子代理退出后其日志也无人回看 |
| journal | 永久 | 有"发生过什么"，但没有"提炼"和"下次怎么用"的路径；新会话不会主动读旧日志 |

所以现状是：**日志即真相只完成了审计，没完成复用。** 模型每次都要从零重新探索同一件事。

### 1.2 三个具体场景

**场景 A：同一仓库反复解释构建/验收命令。**

新开一个会话，模型通常要先 `ls`、读 `Cargo.toml`、看 `tools/`，才知道本仓库的门禁是
`bash tools/ci.sh`、单测是 `cargo test --all`、验收是 `bash tools/acceptance.sh`。
按 `estimate_tokens` 的计价，一次这样的探索约 6 次工具调用、2k–4k tokens；如果一天开 10 次
会话，就是 2 万–4 万 tokens 花在"重新认识同一个仓库"上。这些命令在旧日志的
`tool/request` 里躺着，但下一次没有任何代码去读它。

有记忆之后，只需要一条 `build` 记忆：

```
门禁 bash tools/ci.sh；单测 cargo test --all；验收 bash tools/acceptance.sh
```

按 CJK 0.85 tok/字符估算，约 40 tokens；一轮 10 步也只多 400 tokens，远小于重新探索。

**场景 B：反复踩同一个坑。**

Windows 上通过 WSL argv 桥传中文会乱码，正确做法是 `--task-file`（`docs/02` 末尾已记录，
但只对读过文档的人有效）。每个新会话的模型仍会先试 `--task "中文任务"`，拿到乱码/失败，
再被纠正。日志里留下了这次失败，但没有任何机制把它变成下一次的"先验"。

记忆里应该只有一句 `pitfall`：

```
Windows 下中文任务参数经 argv 会乱码；用 --task-file 传 UTF-8 任务文件
```

注意这条记忆来自**人的结论**（或多次 repair 的结构化统计），不是模型当时的自由发挥。

**场景 C：跨会话丢失偏好。**

用户在 `default` 会话里说过："文档一律中文；不要跑 git；只写必要文件；改完跑
`bash tools/ci.sh`。" 这些偏好没有写进 `CORE`。`ui sessions --new work` 切过去之后，
`switch_journal` 会清空 history（这是正确的，防止数据泄漏），偏好随之消失；
`--append-system-prompt` 又必须每次调用都重复传，而且快照只记录段大小、不记录原文，
事后无法审计"当时到底要求了什么"。

记忆里存一条 `preference`、`scope=repo`、`expiresAt=0`（永不过期）、`confidence=1.0`，
就能跨会话、跨进程生效，并且能被 `ui memory show` 原样回看。

### 1.3 为什么不是"把历史全带上"

- 预算是 20 万 tokens，`compact` 在 1.2 万 tokens 就折叠；历史会一直涨，且每一步都重发一次。
- 跨会话根本没有"历史"可带：会话就是另一个 JSONL（`docs/20`）。
- 偏好、坑、构建命令是**稀疏**的；全量历史里 99% 是当时的探索噪声，带进来只会稀释注意力。
- 需要一个小的、排过序的、会过期的子集：这正是本文要设计的东西。

## 2. 设计目标与非目标

### 2.1 目标

| 编号 | 目标 | 怎么证明（可证伪） |
|---|---|---|
| G1 | 跨会话保留少量事实/偏好/坑 | 写一条记忆，换 session/换进程后 `ui run` 的快照里仍有 `memory` 段 |
| G2 | 每条记忆可追溯到具体事件 | `ui verify` 校验 `sourceSeq/sourceHash/sourceJournal`；篡改来源事件后 exit 2 |
| G3 | 读取确定、可计价、有上限 | 同一组记忆 + 同一 query 两次检索结果 id 顺序一致；`memory` 段 tokens 计入快照 |
| G4 | 写入不会污染上下文 | 自动提炼只产候选；候选不 `accept` 不进快照（测试断言） |
| G5 | 能删除、能纠错 | `ui forget` 后下一轮不再注入；旧 `memory/used` 仍可验证 |
| G6 | 零依赖、全本地 | `Cargo.toml` 不变；无网络调用；只用 `json`/`sha256`/`time`/`Journal` |
| G7 | 成本有界 | 默认 top-5 / 300 tokens；记忆文件 ≤ 2 MB、live ≤ 500 条；超限写入失败而非静默截断 |

### 2.2 非目标（明确不做）

| 编号 | 不做 | 为什么 |
|---|---|---|
| NG1 | RAG / 向量库 / embedding | 引入模型依赖，检索不可解释、不可复现；与"零依赖 + 可审计"冲突 |
| NG2 | 代码库全量索引 / 代码图谱 | 代码的真相是磁盘上的文件；要读就 `fs_read`/`grep`，索引会过期且成本高 |
| NG3 | 云端同步 / 共享记忆服务 | 只做本地文件；跨机器复制 `.ui` 不是支持路径 |
| NG4 | 隐式学习（模型自己决定记住什么） | 模型不能写记忆；自动提炼只从结构化事件产**候选**，候选不改变任何行为，`accept` 才生效 |
| NG5 | 用模型摘要生成记忆 | 多一次调用、多一个失败点、不可复现；模板化文本足够 |
| NG6 | 自动消解矛盾 | 同 `(scope,key)` 冲突时失败退出 3，由人选择 `--replace` 或换 `--key` |
| NG7 | 跨仓库全局记忆 | 记忆是 workspace 本地；避免 A 项目的事实泄漏到 B 项目 |
| NG8 | 加密删除 / 物理擦除 | append-only 日志不重写；`forget` 是"不再使用"，不是"从磁盘抹掉"（见 §6.6） |

## 3. 数据模型

### 3.1 文件与格式

```
<ws>/.ui/memory.jsonl     ← 记忆事件（本设计新增）
<ws>/.ui/memory.lock      ← Journal 自己的写锁（沿用 journal.rs 的规则）
<ws>/.ui/memory.guard     ← Store 的"检查 + 追加"外层锁（见 §4.5）
```

**为什么复用 `Journal`：**

1. 记忆写入本身就是"一次副作用一条日志"，和契约 5 同构；`Journal::append` 已经做到
   先落盘再返回、每条一次 `fsync`、`prev/hash` 链、多进程 `flock`、尾部撕裂修复。
2. `ui verify` 的流式校验器可以直接跑在记忆文件上，不需要第二套完整性代码。
3. 追加式 JSONL 对人类可读、可 `grep`、可 `replay`，且没有 schema 迁移问题。
4. 规模小：live ≤ 500 条、文件 ≤ 2 MB，全量加载物化的成本可以忽略（见 §5.8）。

**为什么按 workspace 而不是按 session：** 记忆要解决的就是"换会话还记得"；按 session
存等于把问题原样保留。文件在 `.ui/` 下，已被 `.gitignore` 忽略，也不会被
`isolate` 复制（`ISOLATE_SKIP` 含 `.ui`）、不会被 `observe::scan` 扫到（`SKIP_DIRS` 含 `.ui`）。

**谁能写：** 只有内核/CLI（`ui remember`、`ui forget`、`Store::propose/accept`）能写。
模型侧工具不能写：见 §7.6 的策略加固。子代理（workflow 的并行进程）一律带
`--no-memory` 启动，只有父进程能写，避免子代理把自己的 repair 候选灌进父记忆。

### 3.2 事件类型

| 事件 `kind` | 作用 | 是否进入上下文 |
|---|---|---|
| `memory/write` | 一条记忆被显式写入（可带 `replaces` 原子替换） | 是（live） |
| `memory/candidate` | 自动提炼出的候选（repair / delegation） | 否，直到 `accept` |
| `memory/accept` | 把候选提升为 live（可带 `replaces`） | 提升后是 |
| `memory/forget` | 墓碑：按 id 列表停止使用（live 或候选） | 否 |
| `memory/used` | 审计：这一轮实际注入了哪些 id、分数、tokens | 只读元数据，不回灌 |
| `memory/reject` | 写入/候选因冲突被拒绝（只记 attempted/existing id，不记被拒正文） | 否 |
| `journal/torn` | `Journal::open` 修复撕裂尾巴时自动追加，物化时忽略 | 否 |

**命名约束（重要）：** `Journal::append` 会把 payload 的字段直接合并进事件，而事件本身
已经有 `kind`。所以记忆的"种类"字段必须叫 **`memKind`**，不能叫 `kind`，否则会和
事件类型撞名，`stream_verify::scan_fields` 也会看到重复键。同理，`source` 相关信息用
`sourceKind/sourceSeq/sourceHash/sourceJournal` 四个扁平字段，不嵌套对象。

### 3.3 字段

**存储字段（`memory/write` / `memory/candidate` 的 payload）：**

| JSON 字段 | Rust 字段 | 类型 | 说明 |
|---|---|---|---|
| `id` | `id: String` | `m-` + 12 hex | 内容寻址：`sha256(memKind \x1f scope \x1f key \x1f text)` 前 12 位；同内容同 id，天然去重 |
| `memKind` | `mem_kind: MemKind` | `build` / `pitfall` / `preference` / `fact` / `convention` | 决定默认 TTL、检索加成、自动提升白名单 |
| `scope` | `scope: String` | `repo` 或 `path:<rel>` | `path:` 后的相对路径必须规范化、在 workspace 内、不能是 `.ui`/`secrets`/`..` |
| `key` | `key: String` | 空，或 `[a-z0-9][a-z0-9_.:/-]{0,63}` | 矛盾检测的"主题键"。非空时同 `(scope,key)` 只允许一条 live；为空时不做硬键冲突，只做近似重复检测（§6.4）。自动提炼总是给 key |
| `text` | `text: String` | ≤ 240 字符 | 单行、空白折叠、拒绝控制字符；写入时做密钥扫描 |
| `tags` | `tags: Vec<String>` | ≤ 8 个，每个 `[a-z0-9_-]{1,24}` | 参与检索；小写去重 |
| `sourceKind` | `source_kind: SourceKind` | `cli` / `repair` / `delegation` | 来源类型白名单 |
| `sourceJournal` | `source_journal: String` | 路径（`/` 分隔） | 来源事件所在的 journal；`cli` 为空 |
| `sourceSeq` | `source_seq: u64` | ≥ 0 | 来源事件的 `seq`；`cli` 为 0（来源就是本条 write 事件自身） |
| `sourceHash` | `source_hash: String` | 64 hex 或空 | 来源事件的 `hash`；用于防"来源被换掉" |
| `expiresAt` | `expires_at: u64` | 0 = 永不过期 | 毫秒时间戳；默认 TTL 见 §3.6 |
| `confidence` | `confidence: f64` | 0.0–1.0 | 显式 1.0；自动候选 0.7–0.9；保留两位 |
| `replaces` | `replaces: Option<String>` | 可选 | 同一条 `memory/write`/`memory/accept` 内原子替换旧 id |
| `evidence` | `evidence: Vec<u64>` | 可选，≤ 8 | 自动候选的证据 seq 列表（只放 seq，不放原文） |
| `conflictWith` | `conflict_with: Option<String>` | 可选 | 候选已知与某条 live 冲突时标记，供 review 展示 |

**派生字段（物化时计算，不写进事件）：**

| 字段 | 来源 | 用途 |
|---|---|---|
| `createdAt` | 对应事件的 `ts` | 排序、展示 |
| `writeSeq` | `memory/write` 或 `memory/accept` 的 `seq` | "这条记忆是在第几条事件生效的" |
| `hits` | 所有引用它的 `memory/used` 的条数 | 展示、排序 tie-break；**不自动提高置信度** |
| `lastHitAt` | 最后一次 `memory/used` 的 `ts` | 展示"多久没用了" |
| `forgottenAt` | `memory/forget` 或 `replaces` 的 `ts` | `ui memory list --all` 展示 |
| `acceptedBy` | `memory/accept.by` | `human` / `policy`，审计自动提升 |

**为什么 `hits/lastHitAt` 不存成可变字段：** 命中是"发生过的事件"，不是计数器。存成可变字段
就要在每次检索后原地改写记忆文件，既破坏 append-only，也会让"命中次数"变成一份无法回放的
自述。这里让 `memory/used` 做唯一事实来源，`Store::refresh` 重放时算出 `hits/lastHitAt`；
测试 `hit_counts_are_replayed_not_mutated` 会断言"连续 refresh 两次，内存里的 hits 相同、
文件字节不变"。

### 3.4 事件示例

一条显式记忆（字段顺序即 `Json::obj` 顺序；`prev/hash` 省略展示）：

```json
{"seq":1,"ts":1763380800000,"kind":"memory/write","id":"m-3f2a9c1b7d04","memKind":"build","scope":"repo","key":"build.verify","text":"门禁 bash tools/ci.sh；单测 cargo test --all；验收 bash tools/acceptance.sh","tags":["ci","test"],"sourceKind":"cli","sourceJournal":"","sourceSeq":0,"sourceHash":"","expiresAt":0,"confidence":1,"replaces":null,"w":"1a2b3c4d","prev":"0000000000000000000000000000000000000000000000000000000000000000","hash":"…"}
```

一条自动候选：

```json
{"seq":4,"ts":1763380920000,"kind":"memory/candidate","id":"m-9a1c4e2f0b77","memKind":"pitfall","scope":"repo","key":"protocol:json-args:fs_write","text":"工具 fs_write 的参数在 2 个轮次里出现同类 JSON 错误（expected ',' or '}'）；调用前先确认参数是完整 JSON 对象。","tags":[],"sourceKind":"repair","sourceJournal":".ui/journal.jsonl","sourceSeq":42,"sourceHash":"8c1d…","evidence":[18,42],"confidence":0.7,"expiresAt":1794916800000,"conflictWith":null,"w":"1a2b3c4d","prev":"…","hash":"…"}
```

一次使用记录：

```json
{"seq":9,"ts":1763381000000,"kind":"memory/used","turn":3,"journal":".ui/journal.jsonl","snapshotId":"a1b2c3d4e5f60718","query":"修一下构建脚本","ids":["m-3f2a9c1b7d04"],"scores":[2.417],"tokens":86,"considered":12,"w":"1a2b3c4d","prev":"…","hash":"…"}
```

一次遗忘：

```json
{"seq":10,"ts":1763381100000,"kind":"memory/forget","ids":["m-3f2a9c1b7d04"],"reason":"用户要求删除","w":"1a2b3c4d","prev":"…","hash":"…"}
```

### 3.5 物化与状态机

`Store::refresh()` 用 `Journal::load` 读出全部事件，按 `seq` 升序重放：

| 事件 | 动作 |
|---|---|
| `memory/write` | 校验 id 与内容一致；若 `replaces` 有值，先给旧 id 记墓碑；插入 live（同 id 覆盖只刷新有效期/置信度，不产生重复条目）；若同 `(scope,key)`（key 非空）已有**别的** live → 返回错误（fail closed） |
| `memory/candidate` | 若 id 已在 live 或已墓碑 → 忽略；否则插入候选（同 id 覆盖） |
| `memory/accept` | 候选必须存在；若 `replaces` 有值，先墓碑旧 id；把候选移入 live；同 key 冲突则报错 |
| `memory/forget` | 对 `ids` 中每个 id：从 live/候选移除，记 `forgottenAt` |
| `memory/used` | 对 `ids` 中每个 id：若此刻是 live → `hits += 1`、`lastHitAt = ts`；否则只记录悬空引用（`ui verify` 判失败，`refresh` 不因此失败） |
| `memory/reject` / `journal/torn` / 其他 | 只计数，不改变状态 |

**不变量：** `key` 非空时，同一 `(scope,key)` 最多一条 live。`Store::refresh` 遇到违反直接返回
`Err("记忆冲突：...")`，`run_turn` 以 `AgentError::Memory` 失败——不静默挑一条用。

### 3.6 上限与默认 TTL

| 项 | 默认 | 超限行为 |
|---|---|---|
| live 条数 | 500 | `ui remember` 失败退出 1，提示先 `ui forget`；不静默淘汰 |
| 候选条数 | 200 | 新候选写 `memory/reject`，提示先 review |
| 记忆文件大小 | 2 MB | 写入前检查，失败退出 1 |
| `text` 长度 | 240 字符 | 校验失败退出 1 |
| 单轮注入 tokens | 300 | 按 top-k 截断；不超 |
| top-k | 5 | 固定 |
| `confidence` 下限 | 0.30 | 低于此值不注入（仍可在 `ui memory list --all` 看到） |

| `memKind` | 默认 `expiresAt` | 理由 |
|---|---|---|
| `build` | now + 180 天 | 构建/验收命令会变，半年后应重新确认 |
| `pitfall` | now + 365 天 | 坑通常更稳定，但一年后可能已被修 |
| `preference` | 0（永不） | 偏好是用户意志，不该自动过期 |
| `fact` | now + 90 天 | 仓库事实变化快 |
| `convention` | now + 365 天 | 约定比事实稳定 |

## 4. 写入路径

### 4.0 共通闸门

所有写入（显式、repair、delegation）都走同一个内部函数 `Store::append_event`，顺序固定：

1. **规范化**：`text` 去首尾空白、把 `\r\n`/`\n` 折叠成空格、拒绝控制字符；`scope` 的
   `path:` 后缀 `\`→`/`、去掉 `./`、拒绝绝对路径/`..`/`.ui`/`secrets`；`key` 小写。
2. **密钥扫描**：命中 `api_key=`、`token:`、`sk-`、`ghp_`、`AKIA`、`-----BEGIN`、
   长 hex（≥32）或 `secrets/` 子串 → 拒绝，退出 1，**不写任何事件**（避免把疑似密钥
   留在 `memory/reject` 里）。
3. **算 id**：`memory::id_for(mem_kind, scope, key, text)`；同 id 已 live 且未过期 → 幂等成功，
   打印"已存在 seq=N"，**不追加事件**；同 id 已 live 但已过期 → 追加一条同 id 的
   `memory/write` 刷新 `expiresAt`/`confidence`（物化时同 id 覆盖，不算冲突）；同 id 在候选 →
   按本条 `memory/write` 提升为 live。
4. **拿外层锁**：`journal::with_file_lock(<ws>/.ui/memory.guard, ...)`，把"重新加载 + 检查 +
   追加"做成一个临界区（§4.5）。
5. **重新加载**：锁内 `Journal::load` + `Store::materialize`，确保看到别的进程刚写的事件。
6. **冲突检查**（两级，见 §6.4）：
   - 硬键：`key` 非空且同 `(scope,key)` 已有别的 live，本条又没带 `replaces` → 写
     `memory/reject` 并返回冲突（退出 3）；
   - 近似重复：两条 `key` 都为空、同 scope 同 `memKind`、token Jaccard ≥ 0.35 且共享
     token ≥ 2 → 同样拒绝，提示用 `--replace` 或显式 `--key` 区分。
7. **追加事件**：`Journal::open(memory.jsonl).append(kind, payload)`；`append` 内部再拿
   `memory.lock`（不同文件，锁序见 §4.5），`fsync` 后返回。
8. **更新缓存**：把新 entry 放进 `Store` 的内存状态，供本进程后续轮次使用。

失败语义：显式写入失败 → 非零退出；自动候选写失败 → `run_turn` 返回
`AgentError::Memory`（不静默），因为"决定要提炼"之后写不进去就是审计缺口。

### 4.1 显式 `ui remember`

触发：用户执行命令的那一刻。它是唯一能让人写的自由文本进入记忆的入口。

```
ui remember --text "门禁 bash tools/ci.sh；单测 cargo test --all" \
            --kind build --scope repo --key build.verify \
            --tags ci,test --expires 180d
ui remember --text-file note.txt          # Windows 中文参数走 argv 会乱码，用文件
ui remember --text "..." --replace m-3f2a9c1b7d04
```

- `--kind` 默认 `fact`；`--scope` 默认 `repo`；`--key` 默认为空（建议给 `build`/`fact`
  一个稳定 key，如 `build.verify`）；`--expires` 支持 `30d`/`12h`/`never`，默认按 §3.6；
  `--confidence` 默认 1.0，只允许 0–1。
- `--replace <id>`：写一条 `memory/write`，payload 带 `replaces`；物化时旧 id 记墓碑、
  新 id 生效，一条事件完成替换，不会出现"旧的已删、新的没写"的中间态。
- 输出：`已写入 m-…（seq 12，build/repo，180 天后过期）`；冲突时打印两条 id 和
  `ui remember --replace <旧 id> ...` 的建议，退出 3。
- **反污染**：只接受人写的文本；有长度/密钥/scope 校验；同 key 唯一；重复写幂等。

### 4.2 从 `model/repair` 提炼

来源事件（Phase 2 起 `model/repair` 增加 `turn` 字段）：

```json
{"kind":"model/repair","attempt":1,"tool":"fs_write","error":"expected ',' or '}' at byte 61","raw":"…","turn":3}
```

规则：

- **触发**：`Agent::finish` 在写完本轮 `turn/end` 之后调用
  `memory::distill_repairs(&self.journal.events, turn, journal_path)`；只处理刚结束的这一轮，
  不回头扫描全部历史，避免一次开启就把旧日志全变成候选。
- 只读 `model/repair`，**不读** `model/response.content`、不读 `raw`。
- `error` 先归一化成 `error_class`：截到 `" at byte "` 之前、去掉数字、小写、最长 48 字符。
  这样两次不同偏移的同一种错误会归到同一类。
- 按 `(tool, error_class)` 分组；要求**至少 2 个不同 turn** 出现，才产候选。
- 文本是模板，只填结构化字段：

  ```
  工具 {tool} 的参数在 {n} 个轮次里出现同类 JSON 错误（{class}）；调用前先确认参数是完整 JSON 对象。
  ```

- `memKind=pitfall`，`scope=repo`，`key=protocol:json-args:{slug(tool)}`（`slug` 把非
  `[a-z0-9_]` 字符换成 `_`，例如 `(protocol)` → `protocol`），
  `confidence=0.7 + 0.1 * min(n-2, 2)`（0.7/0.8/0.9），
  `sourceKind=repair`，`sourceSeq/sourceHash` 取该组最后一条 `model/repair`，
  `evidence` 取该组 seq（最多 8 个）。
- 只写 `memory/candidate`，不写 `memory/write`；`ui memory review` 里可见，`accept` 后才注入。

**反污染：** 阈值（≥2 个 turn）过滤偶发错误；只用 `tool`/`error_class`/计数，不存模型原文；
内容 id 去重（同一模式不会越写越多）；候选不参与检索；与 live 冲突时只记 `conflictWith`，
不覆盖；显式 `--no-learn` / `UI_MEMORY_LEARN=0` 可关。

### 4.3 从 delegation 结果提炼

来源事件只有两类，且都在父日志：

```json
{"kind":"delegate/end","id":"a1","ok":true,"summary":"…","journal":"…","workdir":"…","changed":["src/a.rs"]}
{"kind":"team/accept","cmd":"bash tools/ci.sh","ok":true,"code":0,"tail":"…"}
```

规则：

- 只有配置了 `ui team --accept "<cmd>"` 才有 `team/accept`；没有验收命令就没有这条记忆——
  因为 `delegate/end.summary` 是子代理的**自述**，不可信（`docs/02` 已有"声称写了文件、
  `changed` 却是空"的实例）。
- 成功：`team/accept.ok == true` → 候选 `build`，
  `key="verify:" + sha256(cmd)[..8]`，文本
  `验收命令 \`{cmd}\` 在此工作区通过（exit 0）。`，`confidence=0.8`。
- 失败：`team/accept.ok == false` → 候选 `pitfall`，文本
  `验收命令 \`{cmd}\` 在此工作区失败（exit {code}）。先跑它复现，再改。`，`confidence=0.7`。
- **不存 `tail`**（命令输出可能含密钥/路径）；`evidence` 取 `delegate/end` 的 seq（≤8），
  `sourceSeq/sourceHash` 取 `team/accept` 事件。
- 只由 `cmd_team` 在 append `team/accept` 之后调用 `memory::distill_delegation(...)`；
  子代理进程带 `--no-memory`，不写父记忆。

**反污染：** 断言优先于自述（只信退出码与产物）；不存命令输出；只在 `team` 模式（有显式
`--accept`）产候选；`key` 由命令哈希定位，重复执行同一验收命令只会得到同一个 id；候选待审。

### 4.4 三条写入路径的反污染对照

| 机制 | `ui remember` | repair 提炼 | delegation 提炼 |
|---|---|---|---|
| 来源白名单 | 人 | 只读 `model/repair` 的结构化字段 | 只读 `team/accept` + `delegate/end` 的 ok/changed |
| 不存模型/子代理原文 | 是（人写的） | 不存 `raw` | 不存 `summary`/`tail` |
| 阈值 | 无（人负责） | ≥2 个 turn | 必须有 `--accept` |
| 去重 | 内容 id | 内容 id | 命令哈希 key |
| 冲突 | 退出 3，`--replace` / `--key` | 候选标 `conflictWith` | 同左 |
| 激活 | 立即 | 必须 `accept` | 必须 `accept` |
| 密钥扫描 | 是 | 是（字段本身风险低，仍跑） | 是 |
| 上限 | 240 字符 / 500 条 | 同左 | 同左 |
| 可关闭 | 不写就没有 | `--no-learn` | 不配 `--accept` 就没有 |

### 4.5 并发与锁序

`Journal::append` 已经有 `memory.lock` 的 `flock`，但**检查冲突**需要"读全量 + 判断 + 写"
在同一临界区。为此在 `journal.rs` 增加一个很小的公共辅助：

```rust
pub fn with_file_lock<T>(lock_path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T>
```

它复用现有的 `FileLock`（Unix `flock`，其他平台锁文件 + 60s 过期）。`Store` 在拿 guard
之前先 `create_dir_all(<ws>/.ui)`（`Journal::open` 也会建，但 guard 比它先打开）。写路径：

```
acquire .ui/memory.guard
  Journal::load(.ui/memory.jsonl)   // 只读，不拿锁
  materialize / 检查冲突
  Journal::open(.ui/memory.jsonl)   // 拿 .ui/memory.lock
  append(...)                       // 释放 .ui/memory.lock
release .ui/memory.guard
```

锁序固定为 `memory.guard → memory.lock`，代码里没有反向路径，不会死锁。`remember`、
`forget`、`propose`、`accept` 都走同一把 guard；两个 `ui remember` 并发时，第二个在 guard
上等，进来后能看到第一个刚写的事件，于是走"幂等/冲突"分支，而不是两条 live 同时存在。
测试 `two_writers_remembering_the_same_key_leave_one_live_entry` 用两个线程各写一次，
断言 `live == 1`、`reject == 1`。

## 5. 读取路径

### 5.1 时机与查询构造

**每轮只读一次**，在 `Agent::run_turn` 的最前面（`turn += 1` 之前）：

```rust
if self.memory_enabled {
    self.memory.refresh().map_err(AgentError::Memory)?;   // 失败即失败，不静默空记忆
}
self.turn += 1;
self.journal.append("turn/start", …)?;
let query = memory::build_query(input, self.goal.as_deref(), &self.history);
self.memory_hits = if self.memory_enabled && self.memory_tokens > 0 {
    self.memory.retrieve(&query, &memory::Config { top_k: 5, max_tokens: self.memory_tokens, ..Default::default() })
} else { Vec::new() };
let memory_text = memory::render(&self.memory_hits);
self.history.push(Message::user(input));
```

`build_query` 的输入固定为：本轮 `input` + `goal`（若有）+ history 里最近 2 条 `user`
消息（"继续"这类短输入需要上下文才检索得到）。截断到 2000 字符；不调用模型。

开关：`Agent::new` 读 `UI_MEMORY`（`"0"` 关闭，默认开）；CLI 的 `--no-memory` 覆盖它，
`--memory-tokens N` 覆盖默认 300（0 = 不注入）。关闭读取不影响已经存在的记忆文件，
也不影响显式 `ui remember/forget`。

**为什么不在每步重检索：** 每步都变会让快照不可 diff，也会让"这轮用了哪条记忆"变成一堆
重复事件。工具结果里出现的新路径不会触发隐式重检索——这与 NG4 一致；需要看另一个查询时，
用 `ui prompt --memory-query` 单独渲染，而不是在轮次中途改上下文。

### 5.2 分词（零依赖，CJK 感知）

`memory::tokenize(text) -> Vec<String>`：

- ASCII 字母/数字/`_`：连续段，长度 ≥ 2，转小写；
- CJK 字符：单字 + 相邻双字（bigram）都入 token——中文没有空格，bigram 足够区分
  "构建"与"构建命令"；
- 其他字符：分隔符；
- 去掉极短 token 和停用词表（`the/a/an/is/are/to/of/in/on/for/with`，
  `的/了/是/在/和/与/把/被/就/也/不/要/请/我/你`）；
- 全部小写，保持插入顺序，`Vec<String>` 即可（500 条规模不需要倒排表持久化）。

query 的路径提取：按空白和标点切分，保留含 `/`、`\` 或以 `.rs/.md/.json/.toml` 等结尾的
片段，`\`→`/`，去首尾引号；用于 scope 加成。

### 5.3 过滤

对每条 live entry：

1. `expiresAt != 0 && expiresAt <= now` → 跳过（计入"过期"）；
2. `confidence < 0.30` → 跳过；
3. `scope == repo` → 保留；`scope == path:P` → 只有 query 的路径与 `P` 有重叠才保留
   （`qp == P`、`qp.starts_with(P + "/")`、`P.starts_with(qp + "/")` 或 basename 相同）；
4. 其余跳过。

### 5.4 打分：BM25（TF-IDF 的长度归一化变体）+ 三条加成

设 live 条数为 `N`，第 `e` 条的 token 数 `dl`（`text + tags + scope`），平均长度 `avgdl`。
对 query 中每个 token `t`：

```
idf(t) = ln(1 + (N - df(t) + 0.5) / (df(t) + 0.5))
tf(t,e) = t 在 e 中出现的次数
score(e) = Σ_t idf(t) * tf(t,e) * (k1 + 1) / (tf(t,e) + k1 * (1 - b + b * dl / avgdl))
k1 = 1.2, b = 0.75
```

然后：

| 加成 | 值 | 目的 |
|---|---|---|
| scope 命中 | `+2.0`（`scope=path:P` 且 query 路径 = P 或落在 P 下） | 文件相关的坑优先于仓库通用事实 |
| 类型加成 | `pitfall`/`preference` `+0.2`；`build` `+0.1` | 纠错和偏好比"事实"更该被看见 |
| 置信度 | `score *= 0.7 + 0.3 * confidence` | 低置信度仍可能命中，但排在后面 |

`score <= 0.0` 的条目直接丢弃（没有任何 token 重叠）；没有"全量注入"的兜底。

### 5.5 top-k、预算与确定性

```rust
hits.sort_by(|a, b| b.score.partial_cmp(&a.score)
    .unwrap_or(std::cmp::Ordering::Equal)
    .then_with(|| a.entry.id.cmp(&b.entry.id)));   // 同分按 id 升序，保证可复现
```

- 先渲染 header（约 40 tokens），再逐条渲染；加入下一条会超过 `max_tokens` 就停（**不截断
  条目**，因为被截断的记忆可能语义相反；写入时已限 240 字符，单条一定放得下）。
- 默认 `max_tokens=300`、`top_k=5`。`--memory-tokens 0` 关闭注入（仍可 `ui memory` 查看）。
- 没有命中 → `memory_text=""`，快照里不出现 `memory` 段，也不写 `memory/used`。

### 5.6 渲染与进入快照

`memory::render(&[Hit]) -> String` 的输出（这是 `memory` 段的正文）：

```
记忆（从日志派生、可能已过时；与当前文件或工具结果冲突时，以工具结果为准，不要据此直接改文件）：
- [m-3f2a9c1b7d04] build repo conf=1.00 hits=3 key=build.verify :: 门禁 bash tools/ci.sh；单测 cargo test --all
- [m-9a1c4e2f0b77] pitfall repo conf=0.70 hits=1 :: Windows 下中文任务参数经 argv 会乱码；用 --task-file
```

`prompt.rs` 的 `build_snapshot` 增加第 8 个参数 `memory_text: &str`，在 `goal` 之后、
`history` 之前插入：

```rust
if !memory_text.is_empty() {
    sections.push(Section {
        id: "memory", owner: "memory", text: memory_text.to_string(),
        tokens: 0, bytes: 0, volatility: "turn",
    });
}
```

`Snapshot::to_json` 会自动把它带进 `prompt/snapshot` 的 `sections`（id/owner/tokens/bytes/
volatility），所以"这一步为记忆花了多少 token"是可计价的。需要同步改的调用点：

- `ui-core/src/agent.rs`：生产调用；
- `ui-core/src/model.rs` 的 `test_snapshot`；
- `ui-core/src/prompt.rs` 的 4 处测试；
- `ui-cli/src/main.rs` 的 `cmd_prompt`（传 `""` 或按 `--memory-query` 检索后的文本）。

### 5.7 `memory/used` 事件

在**本轮第一条** `prompt/snapshot` 落盘之后写（这样 `snapshotId` 是真实存在的）：

```json
{"kind":"memory/used","turn":3,"journal":".ui/journal.jsonl","snapshotId":"a1b2c3d4e5f60718",
 "query":"修一下构建脚本","ids":["m-3f2a9c1b7d04"],"scores":[2.417],"tokens":86,"considered":12}
```

- `journal` 记录这一轮写进了哪份主日志（会话切换后否则无法核对 `snapshotId`）。
- `considered` 是打分时看过的 live 条数（不是"候选记忆"数）。
- `tokens` 取快照里 `id=="memory"` 段的 tokens（不是重新估算），保证账能对上。
- `scores` 四舍五入到 3 位小数，避免浮点噪声。
- `query` 截断到 200 字符（不含 history 拼接后的全文）。
- 只在至少注入 1 条时写；"检索了但没命中"不写，避免每轮多一条空事件。
- 一轮一条，不按步重复；同一个 `snapshotId` 可以反查这一步的段大小。

### 5.8 复杂度与性能

| 步骤 | 复杂度 | N=500、query=100 tokens 的估算 |
|---|---|---|
| 加载 + 物化 | O(E)（E = 事件数） | 文件 ≤ 2 MB，约 1–3 ms |
| 建 df/avgdl | O(N × avgdl) | 500 × 40 = 2 万次哈希查找 |
| 打分 | O(N × len(q)) | 5 万次 |
| top-k | O(N log k)（k=5） | 可忽略 |
| 总计 | O(E + N×(len(q)+avgdl)) | 单轮 < 5 ms（目标；Phase 3 用 `tools/bench_memory.sh` 钉住） |

没有倒排索引持久化：500 条规模下全量扫比维护索引更简单，也更不容易出现"索引和日志不一致"。
如果将来超过 500 条，正确做法是拒绝写入/让人清理，而不是偷偷加索引。

### 5.9 失败行为

| 情况 | 行为 |
|---|---|
| 记忆文件不存在 | 视为空记忆，正常跑 |
| 哈希链中间损坏 | `Store::refresh` 返回 `Err`；`run_turn` 在写 `turn/start` **之前**返回 `AgentError::Memory`；`ui run` 退出 1 |
| 尾部撕裂 | `Journal::open` 在下次写入时截断并写 `journal/torn`；读取 `Journal::load` 忽略撕裂，正常跑 |
| 同 `(scope,key)` 两条 live（key 非空） | refresh 报错（fail closed），提示跑 `ui verify` |
| 单条字段非法（id 对不上、scope 越界） | refresh 报错；不静默跳过 |
| 会话切换 | 记忆不换（workspace 级），history 照旧清空；测试钉住 |

### 5.10 一个完整读取例子

记忆库里有 3 条：`build.verify`（repo）、`pitfall.windows-argv`（repo）、
`path:src/journal.rs`（"改锁顺序前先读 `with_file_lock`"）。本轮输入
"修一下 src/journal.rs 的锁"，query 含 `src`、`journal`、`锁` 和路径 `src/journal.rs`。
`path:src/journal.rs` 因路径加成排第一；`build.verify` 因 `journal` 不重叠而不命中；
`pitfall.windows-argv` 不命中。最终只注入 1 条，`memory/used.ids` 只有它。

## 6. 淘汰与纠错

### 6.1 命中次数：只读派生，不反馈

- `hits/lastHitAt` 由 `memory/used` 重放得到；`ui memory list` 展示。
- **命中不会自动提高置信度**，也不会自动延长有效期。否则一条被反复注入的错误记忆会自我强化
  （回声室），而"反复命中"只证明它被检索到，不证明它正确。
- 排序里 `hits` 不参与打分，也不参与排序（同分按 id 升序，见 §5.5）；它只用于展示"这条被用过几次"。

### 6.2 过期

- 读取时 `expiresAt <= now` 直接跳过；`ui memory list --all` 里标 `EXPIRED`。
- `ui forget --expired`：把当前所有过期 live/候选的 id 收进一条 `memory/forget`，reason
  `"expired"`；一条事件原子处理，不会写一半。
- TTL 默认见 §3.6；`--expires never` 只对 `preference` 等明确长期有效的类型建议使用。

### 6.3 显式 `ui forget`

```
ui forget m-3f2a9c1b7d04
ui forget --key build.verify --scope repo
ui forget --expired
ui forget --all --yes          # 危险操作，必须 --yes
```

- 选择器必须给一个；一个都没给 → 退出 1，绝不默认全删。
- 没有匹配 → 退出 4，打印"没有匹配的记忆（未写事件）"；脚本能区分"删了"与"没删"。
- 成功 → 一条 `memory/forget {ids, reason}`；打印 id 列表和"下轮起不再注入"。
- `forget` 对候选同样有效；被拒候选也用同一事件清理。

### 6.4 冲突检测

矛盾检测不靠 NLP，靠两级可证伪的规则：

**硬键冲突：** `key` 非空，同 `(scope,key)`、`id` 不同、两条都 live。

```
ui remember --text "cargo test --all"       --kind build --key build.test
ui remember --text "cargo test -p ui-core"  --kind build --key build.test
# 第二条退出 3：同 scope/key，正文不同
```

**近似重复（软冲突）：** 两条 `key` 都为空、同 `scope` 同 `memKind`，且
`token_jaccard(a, b) >= 0.35`、共享 token ≥ 2。这一条覆盖"没写 key 但明显在说同一件事"
的情况，例如：

```
ui remember --kind preference --text "回答一律用中文"
ui remember --kind preference --text "回答一律用英文"
# token 共享 回/答/一/律/用/文 与 回答/答一/一律/律用，Jaccard ≈ 0.63 → 退出 3
# 换成 "回答用中文" 与 "不要跑 git"，共享 token 为 0 → 两条都能存在
```

处理：

- **替换**：`--replace <旧 id>`，一条 `memory/write` 带 `replaces`，旧 id 记墓碑；
- **区分**：给不同的 `--key`（如 `build.test` / `build.test-core`），显式 key 不同即
  跳过软冲突，由人承担"它们确实是两件事"的判断；
- **不提供 `--force` 让两条共存**：同一 key 两条 live 会让检索结果取决于排序细节，
  等于把矛盾藏起来；宁可让人做一次选择。
- 自动候选遇到冲突：写 `memory/candidate` 并带 `conflictWith`，`ui memory review` 显示；
  `accept` 默认拒绝（退出 3），要 `ui memory accept <id> --replace <旧 id>` 才提升。
- 软冲突阈值是设计选择，不是"智能"：它只做词面重叠，误报时用 `--key` 区分即可；
  短句/换词也可能漏检，所以**可能互相矛盾的事实应显式共用同一个 key**（走硬冲突）。
  测试 `near_duplicate_preferences_are_flagged` 与 `unrelated_preferences_coexist` 钉住边界。

### 6.5 纠错流程

| 情况 | 操作 | 结果 |
|---|---|---|
| 记错了 | `ui remember --replace <id> --text "正确内容"` | 一条事件完成替换，旧 id 墓碑 |
| 不该记 | `ui forget <id>` | 停止注入，历史仍可查 |
| 两条重复 | 内容 id 相同 → `ui remember` 幂等，不新增 | live 仍 1 条 |
| 自动候选不准 | `ui memory review` 后 `ui memory reject <id>` | 写 `memory/forget`，候选消失 |
| 候选其实是条好记忆 | `ui memory accept <id>` | 写 `memory/accept`，下轮可注入 |

### 6.6 删除的诚实边界

`forget` 是**逻辑删除**：记忆文件是 append-only，历史事件不会被重写；被忘的正文仍然在
`memory.jsonl`（以及可能的来源 journal）里。原因和日志链一致——允许原地擦除就等于允许
无痕修改历史。因此：

- 密钥/凭据永远不写入（§4.0 的扫描是硬闸）；
- 敏感事实用短 TTL，而不是指望 `forget` 擦盘；
- `ui forget` 的输出只说"不再注入"，不说"已从磁盘删除"；
- 真正的物理擦除需要重写文件并重建链，属于非目标 NG8；如果将来要做，必须是显式的
  `ui memory purge` + 归档旧文件，且 `verify` 能区分"归档"与"篡改"。

## 7. 可审计性

### 7.1 `ui verify` 覆盖记忆文件

`cmd_verify` 在现有 journal 校验之后增加一段（记忆文件不存在则打印"未启用"并跳过）：

```
记忆  .ui/memory.jsonl
      事件 12 · 生效 5 · 候选 2 · 已忘 4 · 过期 1 · 使用 7 · 拒绝 1
      来源 5/5 可追溯（sourceSeq/sourceHash 对得上）
```

任一项失败：

```
FAIL  记忆链损坏 seq=7: hash mismatch
FAIL  记忆 m-9a1c… 引用了不存在的 id（memory/used seq 9）
FAIL  记忆 m-3f2a… 的来源对不上：sourceSeq=42 期望 8c1d… 实际 0a2e…
```

退出码 2（与 journal 损坏一致）。实现上：

- 链：`stream_verify::verify_stream(memory_path)`，与 journal 同一套 O(1) 内存校验；
- 语义：`memory::verify(path, journal_hint, now_ms)` 先物化这份小文件；来源核对按 journal
  分组**流式**扫描（复用 `stream_verify` 的 `trailing_hash`，把 `scan_fields` 提为
  `pub(crate)`），只保留需要的 seq，内存 O(1)，不把大 journal 全量解析成 `Json`。

### 7.2 校验清单（每条都可测试）

| 编号 | 不变量 | 失败含义 |
|---|---|---|
| V1 | `id == memory::id_for(memKind, scope, key, text)` | 有人改了正文但没改 id（或伪造了 id） |
| V2 | `sourceKind != cli` 时 `sourceSeq > 0` 且 `sourceHash` 是 64 hex | 自动记忆没有来源 |
| V3 | 来源 journal 的 `seq` 存在，其 `hash == sourceHash`，且事件类型与 `sourceKind` 匹配（repair→`model/repair`，delegation→`team/accept` 或 `delegate/end`） | 来源被替换/删除/指向别的类型 |
| V4 | 每条 `memory/used.ids` 里的 id，在 `used.seq` 当时是 live（之前 write/accept、之后才 forget 都可以） | 悬空引用；或时间顺序被改 |
| V5 | `key` 非空时，同 `(scope,key)` 最多一条 live；空 key 的近似重复由写入闸拦截 | 冲突被藏进库 |
| V6 | `memory/accept` 的候选存在；`memory/forget` 的 id 存在过 | 状态机被绕过 |
| V7（P3） | 每条 `memory/used` 的 `journal` + `snapshotId` 能对应到更早的一条 `prompt/snapshot`，且其 `tokens` 等于该快照 `memory` 段的 tokens（只对本次 `--journal` 指向的会话做；其它会话只做结构检查） | 使用记录是编的，或账对不上 |

V1 让"改正文"即使重算了文件哈希链也会被内容 id 抓住；V2/V3 把记忆和产生它的事件绑死；
V4 让"这轮用了哪条"可以被历史回放验证。

### 7.3 `memory/used` 与 `ui memory why`

`memory/used` 是"使用记录"，不是"注入内容"。要回答"为什么模型会知道 X"：

```
ui memory why m-3f2a9c1b7d04
```

输出（Phase 3）：

```
m-3f2a9c1b7d04  build/repo  conf=1.00  hits=3
  write   seq 1   2026-11-17T20:00:00Z  source=cli（本事件即来源）
  used    seq 9   turn 3  snapshot a1b2…  score 2.417
  used    seq 21  turn 7  snapshot c3d4…  score 2.103
  source  —       （cli 记忆无外部来源）
```

对自动记忆，`source` 行会打印来源 journal 的 seq 和 hash；对 delegation 记忆还会列
`delegate/end` 的 id 与 `changed` 文件数。这样"这条记忆从哪来、被谁用过"是一条可读的时间线。

### 7.4 回放

- `ui memory log`：按 seq 打印记忆事件（`memory/write`/`candidate`/`accept`/`forget`/`used`/
  `reject`），不解析正文以外的字段，和 `ui replay` 同风格。
- `ui memory show <id>`：打印该条当前物化状态 + 原始事件 JSON + 所有 used 记录。
- `ui replay` 仍只回放主 journal；记忆是另一条链，不混进主 journal 的 seq（混了会破坏
  "一副作用一日志"的序号语义）。需要合并视图时用 `ui memory why`。

### 7.5 篡改演练（测试直接覆盖）

1. 把 `memory.jsonl` 里一条 `text` 改掉 → 链 hash 不匹配 → `ui verify` exit 2；
2. 改 `text` 并重算该行 hash（模拟会写链的攻击者）→ V1 内容 id 不匹配 → exit 2；
3. 删掉一条 `memory/used` → 后续 seq 不连续 → exit 2；
4. 伪造 `memory/used.ids` 指向不存在的 id → V4 → exit 2；
5. 把 `sourceSeq` 指向主 journal 的另一条事件 → V3 类型/hash 不匹配 → exit 2；
6. 给同一 `(scope,key)` 追加第二条 `memory/write` → 物化/校验 V5 → exit 2。

### 7.6 策略加固：模型不能写记忆

`Policy::defaults` 增加两条（只针对记忆文件，不影响 `.ui/todos.json` 等现有路径）：

```rust
Rule::new(FS_READ,  &format!("{}/.ui/memory.jsonl", ws), Decision::Deny)
    .with_note("记忆只能通过快照读取"),
Rule::new(FS_WRITE, &format!("{}/.ui/memory.jsonl", ws), Decision::Deny)
    .with_note("记忆只能由内核/用户写"),
```

原因：`.ui` 目前对工具是可读写的；如果不拦，模型可以 `fs_write .ui/memory.jsonl` 绕过
整条写入闸（虽然会断链，但那是事后检测，不是阻止）。CLI 的 `ui remember` 是内核直接写文件，
不经过工具策略，所以不受影响。plugin/MCP 仍是信任边界（它们能直接写文件），这条只能在
`verify` 层兜底；`docs/02` 已如实说明这个边界，不假装解决。

## 8. 实现计划

三期都遵守：不新增 crate、不改 `Cargo.toml`、不跑网络；每期结束 `bash tools/ci.sh` 必须绿。

### 8.1 第一期：显式记忆 + 读取 + 审计（可独立上线）

**要改的文件：**

| 文件 | 改动 | 预计行数 |
|---|---|---|
| `ui-core/src/memory.rs`（新） | `MemKind/SourceKind/Draft/Entry/Hit/Config`、`id_for`、`validate`、`looks_secret`、`Store::{new,refresh,remember,forget,retrieve,render}`、`build_query`、`tokenize`、`bm25`、`verify`、物化状态机 | 420 产 + 260 测 = 680 |
| `ui-core/src/journal.rs` | `pub fn with_file_lock`（复用 `FileLock`）+ 并发测试 | 25 + 30 = 55 |
| `ui-core/src/lib.rs` | `pub mod memory;` + 导出 `Store/Entry/Hit/Config` | 3 |
| `ui-core/src/prompt.rs` | `build_snapshot` 第 8 参数 `memory_text` + `memory` 段 + 测试调用点 | 25 + 15 = 40 |
| `ui-core/src/agent.rs` | `AgentError::Memory`、`memory/memory_enabled/memory_tokens/memory_hits` 字段、`run_turn` 开头 refresh/retrieve/render、首步 `memory/used`、3 个测试 | 90 + 120 = 210 |
| `ui-cli/src/main.rs` | `cmd_remember`/`cmd_forget`/`cmd_memory` 三个入口与 `dispatch`/`help`；`verify` 记忆段；`doctor` 记忆行；`run/chat/tui/serve/workflow` 的 `--no-memory`/`--memory-tokens`；`prompt --memory-query` | 230 |
| `ui-cli/tests/memory.rs`（新） | 跨进程集成测试 | 240 |
| **合计** | | **≈ 1,460** |

**测试（名字 + 断言什么）：**

| 测试 | 断言 |
|---|---|
| `content_id_is_stable_and_same_text_dedupes` | 同一 draft 两次 `id_for` 相同；`remember` 两次只多 1 条 write 事件，第二次打印"已存在" |
| `retrieval_prefers_the_matching_path` | 两条记忆，query 含 `src/journal.rs`，`path:` 那条排第一 |
| `retrieval_is_deterministic` | 同库同 query 跑两次，返回的 id 顺序完全一致 |
| `retrieval_respects_top_k_and_token_budget` | 10 条记忆、`max_tokens=100`，渲染 tokens ≤ 100 且条数 ≤ 5 |
| `expired_entries_are_not_injected` | `expiresAt=now-1` 的条目不出现，且计入 expired |
| `forget_stops_injection_but_keeps_history` | forget 后 live=0；旧 `memory/used` 仍通过 verify |
| `replace_is_one_event_and_leaves_one_live_per_key` | `replaces` 后同 key live=1，旧 id 有墓碑 |
| `same_scope_and_key_conflicts` | 显式同 `key` 第二次写入返回 Conflict，live 仍 1，有 `memory/reject` |
| `near_duplicate_preferences_are_flagged` | 两条无 key 的近似偏好（"回答一律用中文"/"…英文"）→ 软冲突退出 3，live 仍 1 |
| `unrelated_preferences_coexist` | "回答用中文" 与 "不要跑 git" 共享 token 为 0 → 两条都 live |
| `verify_detects_dangling_used_reference` | 手写 used 指向不存在 id → `VerifyReport.dangling` 非空，CLI exit 2 |
| `verify_detects_forged_source_hash` | sourceSeq 指向主 journal 另一事件 → `bad_sources` 非空 |
| `remember_rejects_secrets` | 含 `sk-`/`API_KEY=`/`secrets/` 的文本退出 1，记忆文件无新事件 |
| `memory_section_enters_the_snapshot_and_memory_used_is_logged`（agent） | 快照 `sections` 有 `memory` 且 tokens>0；`memory/used.snapshotId` 等于该步快照 id |
| `a_corrupt_memory_file_fails_the_turn_before_turn_start`（agent） | 坏链时 `run_turn` 返回 `AgentError::Memory`，journal 里没有新的 `turn/start` |
| `remember_then_run_injects_the_memory`（CLI） | `ui remember` 后 `ui run --mock`，日志有 `memory` 段和 `memory/used` |
| `verify_covers_memory_file`（CLI） | 干净库 exit 0，输出含"生效"与"来源"；篡改后 exit 2 |

**验收：** `cargo test -p ui-core memory`、`cargo test -p ui-cli --test memory`、
`bash tools/ci.sh` 全绿；手工跑一遍 `remember → run → verify → forget → run`。

### 8.2 第二期：自动提炼（候选 + 人工批准）

**要改的文件：**

| 文件 | 改动 | 预计行数 |
|---|---|---|
| `ui-core/src/memory.rs` | `error_class`、`distill_repairs`、`distill_delegation`、`Store::propose/accept/reject`、候选物化、`conflictWith` | 300 + 180 = 480 |
| `ui-core/src/agent.rs` | `model/repair` 增加 `turn`；`finish()` 里 `memory::distill_repairs`（候选）；`memory_learn` 开关；子代理禁用 | 50 + 80 = 130 |
| `ui-cli/src/main.rs` | `cmd_memory review/accept/reject`、`--learn/--no-learn`、`cmd_team` 在 `team/accept` 后调 `distill_delegation`、`run_parallel` 给子进程加 `--no-memory` | 130 |
| `ui-cli/tests/memory_learn.rs`（新） | 提炼与批准 | 220 |
| **合计** | | **≈ 960** |

**测试：**

| 测试 | 断言 |
|---|---|
| `two_repairs_across_two_turns_produce_one_candidate` | 两个 turn 各一次同类 repair → 恰好 1 条 `memory/candidate`，0 条 live，sourceSeq 指向最后一条 repair |
| `one_repair_is_below_threshold` | 单 turn 单次 repair → 0 候选 |
| `different_offsets_share_one_error_class` | `"expected ',' at byte 10"` 与 `"… at byte 99"` 归为同一类，不重复产候选 |
| `candidate_is_not_injected_before_accept` | 有候选时 `ui run` 的快照没有 `memory` 段 |
| `accept_promotes_and_next_turn_injects` | accept 后 live=1，下一轮出现该条，`memory/used` 有它 |
| `delegation_summary_alone_is_never_a_memory` | 只有 `delegate/end.ok=true`、无 `team/accept` → 0 候选 |
| `team_accept_failure_becomes_a_pitfall_candidate` | `team/accept.ok=false` → 1 条 pitfall 候选，文本含 exit code，不含 `tail` |
| `conflicting_candidate_is_queued_not_promoted` | 同 key live 存在 → 候选带 `conflictWith`，accept 退出 3 |
| `child_agent_never_writes_parent_memory` | 并行子进程带 `--no-memory`，父 `.ui/memory.jsonl` 无子代理产生的候选 |

**验收：** 两轮 mock repair 产生候选；`ui memory review` 能看到；accept 后下一轮注入；
`bash tools/ci.sh` 绿。

**工程注意：** Phase 2 起 `Agent::finish` 会写候选，现有 `agent.rs` 里
`repairs_malformed_arguments_instead_of_dying` 等触发 repair 的测试需要在临时 workspace 上
设 `memory_learn=false`（或 `UI_MEMORY_LEARN=0`），否则测试会顺带生成候选文件；这是测试
隔离问题，不是行为改变。

### 8.3 第三期：自动提升策略 + 可解释性 + 质量/性能基准

**要改的文件：**

| 文件 | 改动 | 预计行数 |
|---|---|---|
| `ui-core/src/memory.rs` | `learn_mode = off / candidate / auto`；自动提升白名单（只 `pitfall`/`build`）与阈值（confidence ≥ 0.70、证据 ≥ 2 个 turn）；`memory/accept {by:"policy"}`；`stats`；`why`；mtime/len 缓存 | 220 + 120 = 340 |
| `ui-cli/src/main.rs` | `memory why / stats`、`--learn=auto`、`verify --json`（可选）、`doctor` 细化 | 120 |
| `tools/bench_memory.sh`（新） | 500 条记忆、100 次检索的耗时中位数；A/B 注入 tokens | 70 |
| `ui-cli/tests/memory_audit.rs`（新） | 策略、重放、质量 | 150 |
| **合计** | | **≈ 680** |

**测试：**

| 测试 | 断言 |
|---|---|
| `auto_promote_requires_evidence_from_two_turns` | 只有 1 个 turn 的候选，`--learn=auto` 也不提升；2 个 turn（confidence 0.7）才写 `memory/accept {by:"policy"}` |
| `preference_is_never_auto_promoted` | `preference` 候选在 auto 模式下仍停在候选 |
| `hit_counts_are_replayed_not_mutated` | 连续 refresh 两次 hits 相同，记忆文件字节不变 |
| `memory_used_tokens_match_the_snapshot_section` | 对每个 `memory/used`，其 tokens 等于同 snapshotId 的 `memory` 段 tokens |
| `retrieval_hits_the_labeled_entry_top_1` | 20 条 fixture（query→期望 id），top-1 命中 ≥ 90%，MRR ≥ 0.7 |
| `verify_detects_forged_used_snapshot` | 改 `memory/used.snapshotId` → exit 2 |
| `switch_journal_keeps_memory_but_clears_history` | 切 session 后 memory 仍可检索，history 为空 |

**验收：** `tools/bench_memory.sh` 记录中位数（检索 < 5 ms）；fixture 质量达标；CI 绿。

### 8.4 总账与顺序

| 期 | 生产代码 | 测试代码 | 累计 |
|---|---|---|---|
| P1 | ≈ 790 | ≈ 670 | ≈ 1,460 |
| P2 | ≈ 480 | ≈ 480 | ≈ 2,420 |
| P3 | ≈ 410 | ≈ 270 | ≈ 3,100 |

顺序不能反：没有 P1 的 `memory/used` 与 verify，P2 的候选无法被审计；没有 P2 的候选，
P3 的自动提升没有证据来源。每期都保持"可单独回滚"：`UI_MEMORY=0` 关读取，
`--no-learn` 关提炼，P3 的 `learn_mode` 默认 `candidate`。

**不需要改的：** `journal.rs` 的链/锁/撕裂逻辑（只加一个 `with_file_lock` 辅助）、
`sha256.rs`、`json.rs`、`fasthash.rs`、`policy.rs`（除 §7.6 两条规则）、`isolate.rs`、
`observe.rs`、`budget.rs`（记忆 tokens 走现有 `estimate_tokens`）。

## 9. 与 Qoder 知识引擎的差异

Qoder 的知识引擎是"把代码库整体索引起来"：全量解析、embedding/代码图谱、跨文件语义检索，
回答的是"代码长什么样、这个符号在哪"。我们**不做代码库索引**：代码的真相是磁盘上的文件，
`grep`/`fs_read` 在需要时现读，索引会过期、需要重建、且无法解释"为什么召回这一条"。
我们只做**事件派生记忆**：记的是"我们做过什么、踩过什么、用户要求过什么"，条目少（≤500）、
每条能回指到 journal 的 seq/hash、能过期、能 `forget`、能被 `verify` 逐条核对。两者的
检索对象和可信边界都不同：Qoder 检索代码，我们检索**关于我们自己的、已经发生过的事实**。

## 10. 风险与开放问题

| 风险 | 影响 | 缓解 |
|---|---|---|
| 记忆过时，模型照做 | 改错文件/跑错命令 | 渲染时明确"以工具结果为准"；TTL；`fact` 90 天；`path` scope 加成让局部记忆优先 |
| 恶意/错误记忆污染 | 长期错误行为 | 模型不能写；自动只产候选；非空 key 的 `(scope,key)` 唯一；`verify` V1/V3；密钥扫描 |
| 候选越积越多 | `review` 成为负担 | 候选上限 200；同 id 去重；`ui doctor` 显示候选数；`reject` 清理 |
| 记忆段每步重发 | 长轮次成本放大 | 默认 300 tokens；`memory/used` 记录真实 tokens；P3 做 A/B，不达标就调小/关掉 |
| 多进程同时写 | 两条 live/链断 | `memory.guard` + `memory.lock` 锁序；并发测试；`verify` V5 |
| 会话/日志切换后来源找不到 | `verify` 假失败 | `sourceJournal` 记录来源文件；verify 按 journal 分组流式查 |
| `.ui` 被外部工具改 | 记忆被篡改 | 链 + V1/V3 检测；`ui verify` 门禁；plugin/MCP 仍可能绕过（如实标注） |
| 用户期待"什么都记得" | 失望 | 文档明确：只记显式/断言-backed 的少量事实；不记聊天原文 |

开放问题（留到实现时定）：

1. `ui remember` 是否支持从 stdin 读（管道），以及 `--text-file -` 的语义；
2. 记忆文件跨平台换行/编码（统一 UTF-8、`\n`，读取时容忍 `\r`）；
3. `ui export` 是否把记忆事件嵌进 HTML（P3 可选，默认不做，避免 seq 混用）；
4. 自动提升的证据是否要区分"同一 turn 内多次 repair"与"跨 turn"（当前只认跨 turn）；
5. 记忆条数接近 500 时，是拒绝新写入还是提示 `ui memory stats` 后由人清理（当前选拒绝）；
6. append-only 的长期增长：`memory/used` 每轮一条，2 MB 大约在 2 万条事件后触顶。
   P3 之前用 `ui memory stats` 观察，之后若需要再做 `ui memory archive`（归档旧链 + 新链的
   显式迁移），不把"重写文件"伪装成普通写入。

## 附录 A：命令速查

```bash
ui remember --text "…" --kind build --scope repo --key build.verify --expires 180d
ui remember --text-file note.txt --kind preference --scope repo
ui remember --text "…" --replace m-3f2a9c1b7d04

ui memory                     # = ui memory list
ui memory list --all          # 含候选/已忘/过期
ui memory show m-3f2a9c1b7d04
ui memory log                 # 记忆事件回放
ui memory review              # 候选列表
ui memory accept m-9a1c4e2f0b77 [--replace m-…]
ui memory reject m-9a1c4e2f0b77
ui memory why m-3f2a9c1b7d04  # P3
ui memory stats               # P3

ui forget m-3f2a9c1b7d04
ui forget --key build.verify
ui forget --expired
ui forget --all --yes

ui run --task "…" --no-memory
ui run --task "…" --memory-tokens 0
ui prompt --memory-query "修构建脚本"   # 打印带记忆段的快照
ui verify                             # 同时校验 journal 与 memory
```

## 附录 B：退出码

| 码 | 含义 | 出现在 |
|---|---|---|
| 0 | 成功 | 所有命令 |
| 1 | 用法错误/校验失败/IO 失败/自动提炼写失败 | `remember`、`forget`、`run` |
| 2 | journal 或 memory 链损坏、悬空引用、来源对不上 | `verify` |
| 3 | 记忆冲突（非空 `key` 相同，或空 key 的近似重复） | `remember`、`memory accept` |
| 4 | `forget` 没有匹配项（没写事件） | `forget` |
| 4 | apply 漂移（现有语义，不冲突，因为不同命令） | `apply` |

## 附录 C：与七条契约的映射

| 契约 | 记忆怎么遵守 |
|---|---|
| 1 一步一快照 | `memory` 是带 owner/tokens/bytes 的独立段；`prompt/snapshot` 记录其大小 |
| 2 一调用一效果 | 本设计不新增模型工具；记忆不进工具面，避免模型借工具写记忆 |
| 3 一效果一决策 | `ui remember` 是内核写 `.ui`，不经过工具策略；模型侧对记忆文件的读写被策略拒绝（§7.6） |
| 4 一预算一账户 | 记忆段走 `estimate_tokens`，计入同一个 token 预算；`max_tokens` 另设上限 |
| 5 一副作用一日志 | 每次写入/遗忘/使用都是 `memory.jsonl` 里一条带链的事件，先落盘再生效 |
| 6 一委派一邮箱 | 父日志的 `delegate/end` + `team/accept` 是唯一提炼来源；子代理不写父记忆 |
| 7 一错误一修复 | `model/repair` 是 repair 提炼的唯一输入；只统计结构化字段，不存模型原文 |
