# 本地 WORM 风格审计存储（L2，可选）

> 不属于 `ui-core`，不改 server/UI；只用 Python 标准库。
> 它把 `/api/audit/export` 的输出按字节存成**内容寻址的不可变对象**，并在保留期
> 到期前拒绝删除。**这是本地 WORM 风格，不是 S3 Object Lock，也不是合规 WORM。**
> 代码在 `adapters/worm/store.py`，门禁在 `tools/verify_worm.sh`。

## 1. 它是什么、不是什么

| 能力 | 本 adapter | S3 Object Lock / 合规 WORM |
|---|---|---|
| 对象按内容寻址、路径由 SHA-256 决定 | 是 | 是 |
| 已有对象不被覆盖 | 是（先校验哈希，绝不覆盖） | 是 |
| 保留期内拒绝删除 | 是（本工具的 `delete`） | 是（服务端强制） |
| 删除后留墓碑、禁止同 id 复活 | 是 | 视实现 |
| 篡改可被发现 | 是（对象哈希 + sidecar + index 链 + 墓碑互检） | 是（服务端校验） |
| 保留策略由服务端强制执行 | **不是**（只有本工具会拒绝） | 是 |
| 可信时间戳 / 法律保留（legal hold） | **没有** | 通常有 |
| 离线复制 / 异地见证 / 对象锁 API | **没有** | 是 |

更准确的说法：**它是“本地文件系统上的对象锁式行为模拟”**。验证能证明“这堆文件
是自洽的、并且本工具在保留期内没删过它”；它不能证明“别的程序/root/运维脚本
没有动过它”，也不能证明“删除发生的时间是真实的”。

## 2. 布局与数据模型

```text
<store>/
  index.jsonl                   append-only 哈希链（0444）
  objects/<sha256>              对象字节（0444）
  objects/<sha256>.meta.json    sidecar 元数据（0444）
  tombstones/<sha256>.json      删除墓碑（0444）
  .lock                         单写者锁（POSIX flock）
```

- 对象 id = 对象字节的 SHA-256，路径 `objects/<id>`；id 就是完整性校验值。
- sidecar 恰好四个字段：

  ```json
  {"sha256":"<id>","size":301,"retentionUntil":1790000000,"createdAt":1789990000}
  ```

- 墓碑文件：

  ```json
  {"deleteSeq":2,"deletedAt":1790000001,"id":"<id>","retentionUntil":1790000000,
   "sha256":"<id>","size":301}
  ```

- `index.jsonl` 每行一条记录，`op` 是 `put` / `delete` / `tombstone`：

  ```json
  {"seq":1,"op":"put","id":"<id>","prev":"000…0","ts":1789990000,
   "retentionUntil":1790000000,"size":301,"hash":"<记录哈希>"}
  ```

  `delete` 记录保留 `retentionUntil`/`size` 以便和 `put` 对账；`tombstone`
  记录额外带 `deleteSeq`，指回对应的那条 `delete`。字段集合是固定的：多字段、
  少字段、类型不对都按链损坏处理。

## 3. CLI

```bash
# 存入一份导出（保留到某个绝对时间，epoch 秒）
python3 adapters/worm/store.py put --store /srv/ui/worm \
  --file export.json --retention-until 1820000000
# 也可用相对天数（now + N*86400）
python3 adapters/worm/store.py put --store /srv/ui/worm \
  --file export.json --retention-days 365

python3 adapters/worm/store.py get --store /srv/ui/worm <id> --out restored.json
python3 adapters/worm/store.py delete --store /srv/ui/worm <id>
python3 adapters/worm/store.py verify --store /srv/ui/worm
python3 adapters/worm/store.py list --store /srv/ui/worm          # 只列 live
python3 adapters/worm/store.py list --store /srv/ui/worm --all --json
```

- `put` 打印 `OK put id=<sha256> size=<n> retentionUntil=<epoch> new=yes|no`；
- `get` 默认把字节写到 stdout（适合管道），`--out` 写文件；
- `delete` 打印墓碑路径；保留期内拒绝，打印 `FAIL: retention until … has not
  expired`；
- `verify` 通过时打印计数，失败时逐条打印问题；
- `list --json` 输出数组，字段 `id/size/createdAt/retentionUntil/deleted/deletedAt`。

退出码：

| 码 | 含义 |
|---|---|
| 0 | 成功 / verify 通过 |
| 1 | 输入或 I/O 错误（文件不存在、id 未找到等） |
| 2 | 完整性失败或保留策略拒绝（verify 失败、未到期 delete、retention 冲突） |
| 64 | 用法错误 |

`--now <epoch>` 是**测试钩子**，用来在不等待真实时间的情况下验证保留期。它**不在
`--help` 里展示**，而且只有显式设置 `UI_WORM_TEST_CLOCK=1` 时才被接受：否则直接
`FAIL` 并以退出码 2 拒绝（提示"test-only"）。生产和门禁默认都用本机时钟。
它只改本工具的判断与记录，不是可信时间源。

## 4. index 链与 verify

### 4.1 哈希规则

- `prev` 是上一条记录的 `hash`；第一条是 64 个 `0`（创世）；
- `hash = SHA-256(canonical JSON(record 去掉 hash 字段))`，canonical 指定
  `sort_keys=True`、无空格、UTF-8；
- `seq` 从 1 开始连续递增。改任意字段（包括 `ts`、`retentionUntil`、`deleteSeq`）
  都会让该行哈希对不上；改完再重算本行哈希，则下一行的 `prev` 对不上。

### 4.2 verify 检查什么

1. `index.jsonl` 的字段集合、类型、`seq` 连续性、`prev` 链、每行哈希；
2. 每个 live 对象的文件存在、大小等于 index、内容 SHA-256 等于 id；
3. sidecar 存在且四字段与 index/对象一致（`createdAt` 必须等于 put 的 `ts`）；
4. 每个 tombstone 记录的 `deleteSeq` 指回对应的 `delete`，墓碑文件与 index 一致，
   且对象与 sidecar 确实已经消失；
5. 反向检查：`objects/` 里未入链的对象、没有 put 记录的 sidecar、没有 index
   记录的墓碑文件都算失败；
6. 只给提示、不判失败：index/对象权限可写（0444 只是降低误改概率）。

### 4.3 verify 查不到什么

- 把整份 store 重写一遍并重算所有哈希：没有外部见证时，`verify` 会认为新链合法；
- 没有外部记录的 head/count 时，单纯“删掉最后几条 + 对应文件”可能留下一个
  自洽的更小 store——本 adapter 会在文件与 index 交叉检查中发现大多数这种截断，
  但**不是**不可伪造的日志；
- 本机时钟被改：保留期判断跟着变；
- owner/root 改权限、改文件、备份系统回滚：文件权限挡不住。

要跨过这条线，应把 `verify` 的输出、对象 id（或 `index.jsonl` 的 head）记到
store 之外（工单、发布记录、另一套受控系统），或者叠加
[docs/49](49-audit-sign.md) 的签名/锚定。

### 4.4 崩溃一致性

- `put`：先写对象（`O_EXCL`，不覆盖），再写 sidecar，最后追加 index。中途崩溃会
  留下“未入链”的文件，`verify` 会报错；**重跑同一个 `put` 会补齐 sidecar 与
  index**（字节哈希已验证，所以不是覆盖）。
- `delete`：先追加 `delete` + `tombstone` 两条记录，再写墓碑文件，最后删对象与
  sidecar。中途崩溃时 `verify` 会报 “delete has no tombstone” 或残留文件；
  **重跑同一个 `delete` 会补齐**。已经完成的删除再删一次是幂等的，不会追加记录。
- 删除后的 id **不能重新 put**：tombstone 存在即拒绝“复活”，避免用新保留期
  覆盖旧历史。

## 5. 保留期：不是可信时间戳

- 保留期在 `put` 时固定，之后**不能延长也不能缩短**；同一内容用不同
  `retentionUntil` 重放会被拒绝；
- `delete` 只有在 `now >= retentionUntil` 时执行；相等即视为到期；
- 时间来自本机时钟（epoch 秒）。把系统时间往前调，就能提前删除；记录里的
  `ts`/`deletedAt` 也是本机时钟，**不是可信时间戳**；
- 因此“保留期内没删”是对**本工具行为**的陈述，不是对**机器/运维者**的强制。
  需要不可抵赖的时间证据时，应叠加外部时间戳/签名/锚定，并保留送达回执。

## 6. 权限、fsync 与单写者

- 对象、sidecar、墓碑、index 写完后都设为 `0444`；index 追加时临时加 owner
  写位，写完收回。owner/root 仍可 `chmod`，所以这是**降低误改概率**，不是强制只读；
- 每个文件写入后 `fsync`；新建/替换后尽量 `fsync` 父目录（Windows 上目录
  `fsync` 不可用，跳过；文件本身仍然 fsync）；
- POSIX 上用 store 级 `.lock` 的 `flock`：`put`/`delete` 取排它锁，`get`/`verify`/
  `list` 取共享锁；index 追加在锁内以 `O_APPEND` 一次写入一行并 `fsync`；
- Windows 没有 `fcntl`，退化为**单写者约定**：同一时刻只允许一个进程写这个
  store（多个进程同时 put/delete 没有互斥保证）。读操作不受影响；
- index 记录必须以 `\n` 结尾；尾部不完整会被 verify 判为撕裂（退出 2），而不是
  静默接受。

## 7. 如何把 `/api/audit/export` 接进来

导出接口本身见 [docs/46](46-audit-export.md)。典型流程（L2，部署方自己决定
保留期与存放位置）：

```bash
# 1) 取一份导出（token/受信代理按部署配置；不要把密钥写进日志）
curl -fsS -H "Authorization: Bearer $TOKEN" \
  'http://127.0.0.1:7788/api/audit/export?since=0&limit=10000' \
  -o export.json

# 2) 存入本地 WORM 风格 store（默认本机时钟；保留 365 天）
python3 adapters/worm/store.py put --store /srv/ui/worm \
  --file export.json --retention-days 365
# 记录打印出来的对象 id；它同时是这份 JSON 的 SHA-256

# 3) 定期核对（可放进定时任务；失败退出 2）
python3 adapters/worm/store.py verify --store /srv/ui/worm
python3 adapters/worm/store.py list --store /srv/ui/worm --all
```

建议：

- 把 `put` 打印的 id 记到 store 之外（工单/发布记录），否则删库后无法知道原来
  存过哪些对象；`verify` 的输出也应留档；
- 需要“谁签的”时，先用 [docs/49](49-audit-sign.md) 的 `sign_export.py` 对
  `export.json` 签名，再分别把 JSON 与 `.sig` 两个文件 `put` 进 store（一次
  `put` 一个对象）；需要“第几份”时再追加锚定链；
- 本 adapter 不校验导出 JSON 里的 HMAC——HMAC 是内核侧的可选能力，验签策略见
  docs/46；WORM store 只保证“存进去的字节没变”；
- 恢复时先 `get --out`，再按 docs/46 / docs/49 的规则校验哈希与签名；
- `delete` 不要做成自动任务：到期后是否真的删除是合规决策，应由人/流程触发。

## 8. 未实现

- 真正的 S3 Object Lock / 对象存储后端、legal hold、桶级保留策略；
- 可信时间戳、HSM、远程见证、跨机复制；
- 在 server/UI 内自动落 WORM（本 adapter 明确不改 server/UI，接入是外部步骤）；
- 多写者并发（Windows 上就是单写者约定）；
- 自动清理中断 put/delete 留下的未入链文件（`verify` 会点名，需人工处理）；
- 按对象级别的加密与密钥托管。

## 9. 测试

```bash
bash tools/verify_worm.sh
```

覆盖：put 后 get 逐字节一致；重复 put 不覆盖、不追加 index；换保留期被拒绝；
保留期内 delete 失败；到期 delete 成功并写 tombstone；重复 delete 幂等；删除后
不能复活；篡改对象 / sidecar / index（改字段、截断、撕裂）后 verify 失败；
没有 `python3` 时整体 `SKIP`（退出 0）。门禁在 Linux 与 Windows（Git Bash）上
都会跑；Windows 上走单写者路径。
