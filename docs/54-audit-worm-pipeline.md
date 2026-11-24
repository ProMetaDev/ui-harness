# 审计导出到 WORM 的可验证管线（L2，可选）

> 把 `/api/audit/export` 的导出 JSON 串成一条可验证的链路：
> **原始字节 → OpenSSH detached 签名 → append-only 锚定链 → 本地 WORM 风格对象库**。
> 只用 Python 标准库 + 系统 `ssh-keygen`，不引入 Rust crate、npm 或 Python 第三方包。
> 入口是 `tools/audit_export_to_worm.py`，门禁是 `tools/verify_audit_worm.sh`。
>
> **这不是云 WORM，不是 S3 Object Lock，也不是合规存证。** 它把三个 L2 adapter
> 编排成一条可重复执行的管线，并明确每一层能证明什么、不能证明什么。

## 1. 它补的是哪一段

仓库里已经有三块可选能力，各自解决一个问题：

| 层 | 复用文件 | 作用 | 单独使用时缺什么 |
|---|---|---|---|
| 签名 | `adapters/audit/sign_export.py` | 对导出 JSON 的逐字节副本做 OpenSSH detached 签名 | 不知道“签的是第几份” |
| 锚定 | `adapters/audit/anchor.py` | 把导出 SHA-256 追加进 `anchors.jsonl` 哈希链 | 不保存导出本体，无法恢复 |
| 对象库 | `adapters/worm/store.py` | 按内容寻址保存导出字节，保留期内拒绝删除 | 不知道“谁签的”，也无法跨实例排序 |

本管线把三者串起来：**同一份导出的字节**同时成为签名对象、锚点记录的
`sha256`、WORM 对象 id。任何一步的哈希对不上，最后的整体校验都会失败。

它不会在 server/UI 内自动触发，也不改变 `/api/audit/export` 现有的 HMAC 行为；
接入是部署侧的独立步骤（见第 7 节）。

## 2. 产物与布局

给定 `--out-dir DIR --name NAME`，一次成功运行产生：

```text
DIR/
  NAME.json                 原始导出字节（逐字节保存，不重新格式化）
  NAME.json.sig             OpenSSH detached 签名
  .NAME.<random>.tmp/       运行期间的 staging 目录（成功/失败都不保留）
<--anchors>/anchors.jsonl   追加一条 {"seq","sha256","prev","ts"}
<--store>/
  objects/<sha256>          导出 JSON 作为内容寻址对象（0444）
  objects/<sha256>.meta.json
  index.jsonl               追加一条 put 记录
```

三处 SHA-256 必须相等：

```text
sha256(NAME.json 字节) == anchors.jsonl 最后一条的 sha256 == store 对象 id
```

`anchors.jsonl` 与 store 都可以是已经存在的，管线只追加；已有内容先验证再使用。

## 3. 用法

### 3.1 从已保存的导出跑管线

```bash
python3 tools/audit_export_to_worm.py run \
  --export export.json \
  --out-dir /srv/ui/audit-archive \
  --name export-2026-11-24 \
  --key ~/.ssh/audit_ed25519 \
  --allowed-signers /srv/ui/allowed_signers \
  --identity auditor@example.com \
  --anchors /srv/ui/anchors.jsonl \
  --store /srv/ui/worm \
  --retention-days 365
```

- `--name` 默认取 `--export` 的 stem；URL 模式默认用 UTC 时间戳；
  成品已存在时**拒绝覆盖**（审计产物不做隐式覆盖）；
- `--key` 只接受路径；私钥内容不会被读取、打印或写进日志；
- `--allowed-signers` 与 `--identity` 要么都给、要么都不给。给了就在 staging
  和最终产物上各验一次签名；不给则只检查 `.sig` 存在，**不做密码学验签**
  （最终输出会写 `signature=not checked`）；
- `--retention-days N` 等价于 `retentionUntil = now + N*86400`；
  `--retention-until <epoch秒>` 用于固定保留期，重跑时更容易保持幂等；
- `--expect-head` / `--expect-count` 是可选的外部锚点基准，用来发现截断
  （见第 6 节）。

### 3.2 直接从 `/api/audit/export` 拉取

```bash
UI_AUDIT_TOKEN="$TOKEN" python3 tools/audit_export_to_worm.py run \
  --url 'http://127.0.0.1:7788/api/audit/export?since=0&limit=10000' \
  --out-dir /srv/ui/audit-archive \
  --name export-2026-11-24 \
  --key ~/.ssh/audit_ed25519 \
  --anchors /srv/ui/anchors.jsonl \
  --store /srv/ui/worm \
  --retention-days 365
```

- token 按 `--token` > `--token-file` > 环境变量 `UI_AUDIT_TOKEN` 的顺序取；
  **推荐 `--token-file` 或环境变量**，命令行参数可能被同机进程看到；
- 只有 HTTP **200** 且响应体是可解析 JSON 才继续；404/500、超时、非 JSON、
  非对象、`events` 不是数组都会在创建输出目录之前失败；
- 响应体上限 64 MiB（`--export` 文件也按同一上限拒绝）；URL 不允许带用户名/密码（用 token）；
- 不打印 URL、响应头或 token；HTTP 错误只打印状态码与 reason。

### 3.3 事后复核

```bash
python3 tools/audit_export_to_worm.py verify \
  --export /srv/ui/audit-archive/export-2026-11-24.json \
  --anchors /srv/ui/anchors.jsonl \
  --store /srv/ui/worm \
  --allowed-signers /srv/ui/allowed_signers \
  --identity auditor@example.com \
  --expect-head <上次记录到链外的 head>
```

`--sig` 默认是 `<export>.sig`；`--object-id` 默认等于导出字节的 SHA-256；
`--expected-seq N` 要求锚点链第 N 条正好记录这份导出。`verify` 不写任何文件，
只读存储与链。

退出码：

| 码 | 含义 |
|---|---|
| 0 | 管线成功 / 复核通过 |
| 1 | 任何一步失败（HTTP、JSON、签名、锚定、store、复核） |
| 2 | SKIP：没有可用的 `ssh-keygen -Y sign/verify`（L2 可选依赖缺失） |
| 64 | 用法错误 |

## 4. 执行顺序、原子性与失败语义

`run` 的顺序是故意安排的：

1. 探测 `ssh-keygen -Y sign/verify`；不支持就 `SKIP`（退出 2），不降级成“没签也算成功”；
   同时确认 `--key` 路径存在；
2. 读取/下载导出，校验 JSON，算 SHA-256；
3. 在输出目录内的 staging 目录写 `<name>.json`，签名，得到 `<name>.json.sig`；
   如给了 `--allowed-signers`，先在 staging 上验签；
4. 预检 anchors 链（可读、未撕裂、`seq`/`prev` 自洽）与已有 store（`verify` 通过）；
5. WORM `put`（内容寻址、保留期固定；重复 put 只校验不覆盖）；
6. 追加 anchor（同一份导出已经是最后一条时跳过，避免重跑产生重复记录）；
7. 对 anchors 做一次可选的外部 head/count 校验；
8. 用原子 rename 把 staging 里的 JSON 与 `.sig` 提交为最终产物；
9. 在最终路径上再整体复核一次：对象哈希/内容、store index、anchor 链、
   签名（如果给了 allowed signers）。

失败处理：

- **提交前失败**：删除 staging，不产生 `NAME.json` / `NAME.json.sig`；
  新建的输出目录如果空了也会尽量删掉；
- **提交后复核失败**：删掉本次创建的 `NAME.json` 与 `.sig`，退出非零；
- **不会静默成功**：任何一步抛错都返回非零，并打印 `FAIL:`；
- **WORM 与 anchor 是前向不可逆的**：`put` 一旦成功就不能在保留期内删除，
  所以如果 `put` 之后、`anchor`/提交之前失败，store 里可能留下一个对象。
  这不是“看起来成功”的半成品（退出非零、没有最终产物），重跑同一份导出即可：
  - 用**相同的 `--retention-until`**（`--retention-days` 会按新的当前时间重算，
    可能和已存对象的保留期冲突而被拒绝）；
  - anchor 若已是同一条记录会跳过重复追加；
  - 提交最终产物后完成复核。

崩溃窗口同理：进程在 `put`、anchor、rename 之间被杀，重跑即可收敛；已经提交的
对象/锚点不会因为重跑而改变语义。

## 5. 复核到底检查了什么

`verify`（以及 `run` 的最后一步）逐项检查：

1. 导出文件存在、非空、是 JSON 对象；`events` 若存在必须是数组；
2. `sha256(export)` 是 64 位小写十六进制，且 `--object-id`（若给出）与它一致；
3. `store.get(id)` 读回的字节与导出逐字节相同；`store.verify()` 检查：
   index 字段/`seq`/`prev` 链/记录哈希、对象文件大小与内容哈希、sidecar 四字段、
   tombstone 与 index 的对应关系；
4. anchors 文件可读、无撕裂、`seq` 连续、`prev` 链与记录哈希自洽；导出的
   SHA-256 确实出现在链中；给了 `--expected-seq` 时精确匹配该条；
5. 给了 `--allowed-signers`/`--identity` 时，用 `ssh-keygen -Y verify` 对
   `<name>.json.sig` 验签；identity 与 namespace 必须匹配；
6. 给了 `--expect-head`/`--expect-count` 时，用 anchor 的截断规则检查链没有
   比已知基准更短、已知 head 仍然在链中。

它**不**检查：

- 导出内容是否真实、事件是否完整（那是内核 journal 与 `sha256` 字段的事）；
- `/api/audit/export` 的对称 HMAC（见 `docs/46-audit-export.md`，需要 key）；
- anchor 的 `ts` 是否是可信时间（只是本机时钟）；
- 是否有人把整个 store / 整条 anchor 链重写并重算；
- `.sig` 是否属于“应该签这份导出的人”（那是 allowed_signers 的保管问题）。

## 6. 外部 anchor head 仍是截断边界

锚定链本身只保证“相邻记录自洽”。**删掉尾部若干条、保留前缀**仍然是一条
完全合法的链；`verify` 不带基准时会通过。只有把上一次记录的 head 或 count
从链外带回来，才能发现截断：

```bash
# 运行成功后，把 head 记录到链之外（工单、发布记录、另一套只读系统）
python3 adapters/audit/anchor.py head --anchors /srv/ui/anchors.jsonl --print head

# 以后复核时把已知 head 传进来；head 不在链中即判截断
python3 tools/audit_export_to_worm.py verify \
  --export export.json --anchors /srv/ui/anchors.jsonl --store /srv/ui/worm \
  --expect-head <已知 head>
```

`tools/verify_audit_worm.sh` 专门验证了这条边界：同一份导出的锚点被截断后，
不给 head 的前缀链仍自洽；给外部 head 则必须失败。

因此**必须把 head 存到链之外**。链内自己的“最后一条”不能证明自己没有变短。
同理，store 的 `verify` 也挡不住“整库重写并重算所有哈希”；外部记录的对象 id
或 index head 才能把这一点钉住。

## 7. 如何接 `/api/audit/export`

导出接口本身见 `docs/46-audit-export.md`。两种接法：

### 7.1 先 curl，再用 `--export`

```bash
TOKEN_FILE=/run/secrets/ui-audit-token
curl -fsS -H "Authorization: Bearer $(cat "$TOKEN_FILE")" \
  'http://127.0.0.1:7788/api/audit/export?since=0&limit=10000' \
  -o /srv/ui/audit-archive/export.json

python3 tools/audit_export_to_worm.py run \
  --export /srv/ui/audit-archive/export.json \
  --out-dir /srv/ui/audit-archive \
  --name export-2026-11-24 \
  --key ~/.ssh/audit_ed25519 \
  --anchors /srv/ui/anchors.jsonl \
  --store /srv/ui/worm \
  --retention-days 365
```

适合：导出已经由别的系统落盘、或需要先用 `curl --fail` 自己处理 HTTP 语义。

### 7.2 直接 `--url`

见 3.2。管线会自己检查 HTTP 200 与 JSON，失败时不留半成品。

权限与 token：

- `/api/*` 受 token / 受信代理约束；受信代理模式下每个请求还要带
  `X-UI-Proxy-Secret`，且 `auditor`/`admin`/`owner` 可导出，`viewer`/`operator`
  返回 403（以 `docs/46-audit-export.md` 为准）；
- 本管线只发送 `Authorization: Bearer <token>`，不替部署方管理代理头；
  需要代理头的场景请先用受控脚本把导出取到本地，再用 `--export`；
- token 不要进 shell 历史、不要放进 URL 查询串、不要提交到仓库。

### 7.3 建议的归档组合

- 每份导出的 `<name>.json` 与 `<name>.json.sig` 一起保存；
- 把 anchor head、store 对象 id、导出 SHA-256 记到受控系统（工单/发布记录）；
- 需要把签名也放进 WORM 时，用 `adapters/worm/store.py put` 把 `.sig` 作为
  另一个对象单独 put（本管线只把导出 JSON 放进 store，见第 9 节）；
- `delete` 不自动化：到期后是否删除是合规决策，应由人的流程触发。

## 8. 安全与权限

- **token**：只在内存里用于构造 `Authorization` 头；不打印、不写文件、不放进
  错误信息。推荐 `--token-file`（权限 0600）或 `UI_AUDIT_TOKEN`；
- **私钥**：管线只把路径交给 `ssh-keygen -Y sign`；`sign_export.py` 不读取私钥
  内容，权限过松时只打印一条 `WARN`；
- **HTTP**：只接受 `http://` / `https://`；拒绝带用户名/密码的 URL；错误信息
  只包含状态码与 reason，不包含 URL/响应头/token；
- **输出**：已存在的 `<name>.json` / `.sig` 一律拒绝覆盖；staging 目录在输出
  目录内，失败时清理；提交后复核失败会删除本次创建的产物；
- **权限**：WORM 对象/sidecar/index 与 anchors 都尽量设成 `0444`。这只是降低
  误改概率，owner/root 仍可修改，见第 9 节。

## 9. 边界：本地 WORM 风格，不是云 WORM

必须把话说准：

- **不是 S3 Object Lock / 合规 WORM**：没有服务端保留策略、没有 legal hold、
  没有对象锁 API、没有跨区复制；
- **不是云 WORM**：没有远程见证、没有可信时间戳、没有 HSM；保留期只是本工具
  在 `delete` 时检查本机时钟；
- `0444`、`O_APPEND`、单写者约定都只是降低误改概率；owner/root/备份系统/
  虚拟化层仍可改文件或改权限；
- Windows 上没有 `fcntl`，同一时刻只允许一个写进程（单写者约定）；
- store 的 `verify` 能发现单点篡改和大多数截断，但挡不住“整库重写并重算链”；
- anchor 的 `ts` 是本机时钟，不是可信时间；外部 anchor head 是截断边界，
  但把 head 存到哪里、由谁保管，是部署决策，不是本工具的承诺；
- 本管线只把**导出 JSON** 放进 store；`.sig`、`allowed_signers` 快照需要
  另外归档（可分别 `put`），否则“当时用哪把公钥验的”会丢失；
- 本管线不校验导出的 HMAC，也不判断事件内容是否真实；它保证的是“这份字节
  从导出到存储没有被换过，且签名/锚点/对象三者对得上”。

要跨过这条线，需要把 anchor head、对象 id、`allowed_signers` 快照和 `verify`
输出送到另一套受控系统（对象锁桶、离线介质、第三方存证），并保留送达回执。

## 10. 未实现

- 云对象锁 / S3 Object Lock / legal hold / 服务端保留策略；
- 可信时间戳、透明日志、远程见证、HSM、跨机复制；
- 在 server/UI 内自动触发（本管线明确是外部 L2 步骤）；
- 多写者并发（Windows 上是单写者约定）；
- 自动清理中断 put/delete 留下的未入链文件（`store verify` 会点名）；
- HMAC 校验、`allowed_signers` 的签名/版本管理、key 轮换与撤销列表；
- 把 `.sig` 与导出绑定成单个对象（当前 `.sig` 是旁路文件，要单独 `put` 才进 store）。

## 11. 测试

```bash
bash tools/verify_audit_worm.sh
```

覆盖：

1. 生成临时 ed25519 key、`allowed_signers` 与 `/api/audit/export` 形状的 fixture；
2. 完整管线跑通，产物逐字节一致，anchor 的 `sha256` == 导出哈希 == store 对象 id；
3. 管线日志不含私钥标记；WORM store 与管线 `verify` 都通过；
4. 篡改导出 / 签名 / 对象 / index 后 `verify` 必须失败；
5. 追加第二份导出后，anchor 截断的边界：纯前缀自洽，给出外部 head 必须失败；
6. 保留期内 `delete` 被拒绝（退出 2），对象与 index 不变；
7. URL 模式：HTTP 200 + JSON 成功、token 不出现在日志、404/非 JSON 失败且
   不留输出目录（本地 `http.server` 不可用时该段显示 skip）；
8. 没有 `python3` 或没有可用的 `ssh-keygen` 时整体 `SKIP`（退出 0）。
