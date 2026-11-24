# 审计导出的非对称签名与锚定链（L2，可选）

> `/api/audit/export` 的内核部分仍然是可选的对称 HMAC（见
> [docs/46](46-audit-export.md)）。本文描述的是**仓库里的可选 L2 工具**：
> 用 OpenSSH 做 detached 非对称签名，再用 append-only 锚定链记录每份导出的哈希。
> 不引入 Rust crate、npm 或 Python 第三方包；只依赖 `python3` 与系统 `ssh-keygen`。
>
> docs/46 的“非对称签名未实现”说的是**内核内置**能力；本 adapter 在 core 之外，
> 可选启用，不改变 `/api/audit/export` 现有的 HMAC 行为。

## 1. 它补的是哪一段

对称 HMAC 的问题是：验签的人必须拿到同一个密钥，因此也能伪造签名。
对审计场景更常见的要求是**公开可验证**：持有公钥的人能验证导出，但无法生成签名。

两条互补的线：

| 层 | 工具 | 作用 | 不能证明 |
|---|---|---|---|
| L2 签名 | `adapters/audit/sign_export.py` | 导出 JSON 的 detached OpenSSH 签名（`.sig`） | 导出内容本身是否真实、完整 |
| L2 锚定 | `adapters/audit/anchor.py` | 把每份导出的 SHA-256 追加进 `anchors.jsonl` 哈希链 | 文件真的不可删除（不是 WORM） |

锚定链解决“这份导出是不是当时那一份、中间有没有被换过”；签名解决“这份导出是谁签的”。
两者都不能替代外部见证（见第 5 节）。

## 2. 依赖、失败方式、退出码

- `ssh-keygen` 需要 OpenSSH **8.1+**（`-Y sign` / `-Y verify`）；
- 没有 `ssh-keygen` 或版本太老：`sign_export.py` 打印 `SKIP:` 并**退出 2**，
  绝不降级成“没签也算成功”；
- `sign_export.py`：0 成功 / 1 验签失败或输入错误 / 2 SKIP / 64 用法错误；
- `anchor.py`：0 正常 / 2 链断裂或被改 / 3 截断或尾部撕裂 / 64 用法错误。

`bash tools/verify_audit_sign.sh` 在没有上述能力时输出 `SKIP` 并退出 0，和仓库里
其他 L2 门禁（Postgres、runner）一致；它不会把“没跑”写成“通过”。

## 3. 非对称签名

### 3.1 生成与保管私钥

```bash
ssh-keygen -t ed25519 -C "audit-2026" -f ~/.ssh/audit_ed25519
chmod 600 ~/.ssh/audit_ed25519
```

- 私钥不放进仓库、不放进 `.ui/`、不通过聊天/日志传递；
- `sign_export.py` 只接收 `--key` 的**路径**，不打印私钥内容；
  如果 key 的权限对 group/other 开放，它只打印一条 `WARN`，不读取内容；
- 有口令的 key 需要交互式终端输入口令；批处理场景请使用受控签名机上的
  短期 key（无口令但权限 0600），或者把签名步骤放在人工/受控环境执行。

### 3.2 `allowed_signers` 与身份

`ssh-keygen -Y verify` 不直接读“公钥文件”，而是读一个 allowed signers 列表：

```text
auditor@example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI... audit-2026
```

格式是 `<principal> <keytype> <base64-pubkey> [comment]`。验签时 `-I` 指定的
identity 必须匹配 principal；`-n` 指定 namespace，签名和验签必须一致
（默认 `ui-audit`）。

公钥可以从私钥导出：

```bash
ssh-keygen -y -f ~/.ssh/audit_ed25519
```

不要把私钥交给审计方；只需要把 `allowed_signers`（公钥列表）随导出一起交付。

### 3.3 签名

```bash
python3 adapters/audit/sign_export.py sign \
  --export export.json \
  --key ~/.ssh/audit_ed25519 \
  --namespace ui-audit
# -> export.json.sig
```

- 默认输出 `<export>.sig`，可用 `--sig PATH` 指定（不能指向导出文件本身）；
- 已存在 `.sig` 时默认拒绝覆盖，确认要换签再 `--force`；
- 签名是在**逐字节副本**上做的，导出文件本身不会被改写（脚本还会在签名前后
  各算一次 SHA-256 做防御性检查）；
- 可选 `--allowed-signers ... --identity ...`：签完立即自检一次。

### 3.4 验签

```bash
python3 adapters/audit/sign_export.py verify \
  --export export.json \
  --sig export.json.sig \
  --allowed-signers allowed_signers \
  --identity auditor@example.com \
  --namespace ui-audit
```

成功打印 `OK verified ...` 并退出 0；identity / namespace / 文件内容任一不匹配
都退出 1。等价的裸命令是：

```bash
ssh-keygen -Y verify -f allowed_signers -I auditor@example.com \
  -n ui-audit -s export.json.sig < export.json
```

### 3.5 签名覆盖的范围

签名覆盖导出 JSON 的**原始字节**，包括空白与字段顺序。下游即使只是
“解析后重新格式化”，也会导致验签失败——这是有意的：审计证据应按原样保存。
`exportedAt` 也在签名范围内；同一批事件在不同时间导出会得到不同签名。
如果只想证明“事件集合没变”，比对导出里的 `sha256` 字段，而不是比对签名。

## 4. append-only 锚定链

### 4.1 记录格式与哈希规则

`anchors.jsonl` 每行一条：

```json
{"seq":1,"sha256":"<导出文件字节的 SHA-256>","prev":"<上一条记录的哈希>","ts":1790000000000}
```

- `seq` 从 1 开始、连续递增；
- `sha256` 是导出文件的 SHA-256（不是记录本身的哈希）；
- `prev` 是**上一条记录**的哈希；第一条是 64 个 `0`（创世）；
- 记录哈希 = `SHA-256(canonical_json({seq,sha256,prev,ts}))`，canonical 指定
  按键排序、无空格、UTF-8。因此 `prev` 链同时覆盖 `seq`、`ts` 和导出哈希：
  改任何一个字段，下一条的 `prev` 就对不上；
- 记录只允许这四个字段；多字段、少字段、类型不对都按链损坏处理。

注意：`prev` 不是“上一条的 `sha256` 字段”。如果只把导出哈希串起来，
改 `ts`/`seq` 不会被发现；所以这里用整条记录的哈希做链。

### 4.2 追加

```bash
python3 adapters/audit/anchor.py append --anchors anchors.jsonl --export export.json
python3 adapters/audit/anchor.py append --anchors anchors.jsonl --export export2.json
```

- 已知哈希时也可 `--sha256 <64hex>`（用于跨机传递已计算好的导出哈希）；
- `--ts` 可指定毫秒时间戳，默认取本机当前时间；
- 追加前会先校验已有链；链断了拒绝继续写，避免把坏链接长；
- POSIX 上用 `flock` 串行化；Windows 上是单写者约定（见边界）；
- 追加采用 `O_APPEND` 单行写入 + `fsync`，并把文件权限尽量设成 `0444`。
  下一次追加会临时加 owner 写位，写完再收回；**owner/root 仍然可以 chmod**。

### 4.3 校验与 head

```bash
python3 adapters/audit/anchor.py verify --anchors anchors.jsonl
python3 adapters/audit/anchor.py head --anchors anchors.jsonl
python3 adapters/audit/anchor.py head --anchors anchors.jsonl --print head
```

- `verify` 校验字段、`seq` 连续性、`prev` 链和记录哈希；退出码 0/2/3；
- `head --print head` 适合脚本取出最后一条记录的哈希；
- `verify --expect-count N`：少于 N 条即判截断（退出 3），多出来是正常追加；
- `verify --expect-head H`：H 必须仍然出现在链中。H 之后继续追加是允许的；
  H 找不到才判“截断或整链被重写”（退出 3）。

运维上应把 `head --print head` 的输出记录到**链之外**（工单、发布记录、
另一套只读系统），否则下一次校验没有比较基准。

### 4.4 截断能查到哪、查不到哪

- 删掉尾部若干条：剩下的前缀仍是一条完全自洽的链，`verify` 单独跑会通过；
  只有把**最后一次已知 head 或 count** 传进来才能发现（退出 3）；
- 改中间条：下一条 `prev` 对不上，退出 2；
- 改最后一条：没有后继记录可以发现它；只有外部记录的 head 能发现；
- 把整条链重写并重算所有 `prev`：没有外部见证时无法发现；
- 文件被截断在一条记录中间：尾部按“撕裂”处理，退出 3，且拒绝继续追加。

这就是“锚定到最后一个已知锚点”的准确含义。不要把它读成“任何删除都能发现”。

## 5. WORM 边界：这不是对象锁

`anchors.jsonl` 的 0444、`O_APPEND`、单写者约定都只是**降低误改概率**，不是强制：

- 不是 WORM 存储，不提供“写入后不可删除/不可覆盖”的保证；
- 不是对象存储的 object lock / legal hold，也没有服务端保留策略；
- owner、root、备份系统、虚拟化层都能改文件或改权限；
- 没有远程见证、没有可信时间戳、没有 HSM；`ts` 只是本机时钟；
- Windows 上 `chmod`/`flock` 语义弱，追加链默认单写者。

要跨过这条线，需要把 head 或整份 `anchors.jsonl` 定期送到**另一套受控系统**
（例如对象锁桶、离线介质、第三方存证），并保留送达回执。那是部署决策，
不在本 adapter 的承诺范围内。

## 6. 测试

```bash
bash tools/verify_audit_sign.sh
```

脚本会：

1. 生成临时 ed25519 key 与 `allowed_signers`；
2. 手写一份 `/api/audit/export` 形状的样例 JSON，签名并验签；
3. 验证 identity / namespace 不匹配时失败；
4. 篡改导出 JSON 后验签必须失败；
5. 连续追加 2 条锚点后校验通过，改中间条必须失败；
6. 验证截断边界：纯前缀自洽，但给出已知 head/count 后退出 3；
7. 没有 `ssh-keygen` 时整体 `SKIP`（退出 0）。

## 7. 未实现

- 远程见证 / 第三方时间戳 / 透明日志；
- HSM、key 托管、自动轮换；
- 多签/阈值签名、撤销列表（`ssh-keygen -Y verify -r` 未接入）；
- 真正的 WORM / 对象锁 / 保留策略；
- 在 `/api/audit/export` 响应时自动签名或自动追加锚点（当前是独立的 L2 步骤）；
- `keyId` 与 OpenSSH key 的自动映射（`keyId` 仍只属于内核 HMAC）。

## 8. key 轮换

推荐按审计周期使用独立 key，并把 `allowed_signers` 也当作受控材料版本化：

1. 新周期生成新 key（新文件、新注释），**不要覆盖旧 key**；
2. 在 `allowed_signers` 中为同一个 principal 追加一行新公钥（允许多行同 principal）；
   也可以使用新 principal，但验签的 `-I` 必须跟着变；
3. 新导出用新 key 签；旧导出继续用旧公钥验；
4. 旧 key 只在保留期内保留公钥行；删除后旧签名将无法再验，
   所以要把**当时的 `allowed_signers` 快照**与导出、签名一起归档；
5. 私钥泄露时：立刻从 `allowed_signers` 移除对应公钥行，换新 key，
   并把轮换事件写进审计记录；已经发出的旧签名需要按保留策略重新评估。

`allowed_signers` 文件本身没有签名，建议同时记录它的 SHA-256 并放进锚定链或
发布记录；否则“公钥列表被换掉”会让旧签名验到另一把 key 上。
