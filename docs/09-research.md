# 科研专项：可复现实验包

目标不是"让 AI 做科研"，而是**让 AI 做出来的科研结果可以被别人验证**。

## 一条硬规则

> **没有产物就没有结论。**（no artifact, no claim）

包里没有的文件，报告里就不该出现由它支撑的数字。

## 一个实验包长什么样

```bash
ui lab pack --workspace ./exp --name baseline --out ./bundles/baseline
```

```
bundles/baseline/
  meta.json       环境指纹（OS / kernel / 架构 / CPU 核数 / Python 版本）、工作区、命令列表
  manifest.jsonl  每个产物一行：路径、sha256、字节数、producedBy{seq, tool}
  artifacts/      产物副本（按工作区相对路径）
  journal.jsonl   原始日志副本（哈希链，可 verify）
  report.md       人读的报告：产物表 + 跑过的命令 + 怎么验证
```

**血缘（producedBy）是从日志里推出来的**：哪个 seq 的工具调用写了这个文件。
不是我们事后标注的，是执行时留下的。

## 校验

```bash
ui lab verify ./bundles/baseline     # 0 = 自洽；3 = 有问题
```

它检查三件事：

1. 每个产物的 sha256 与清单一致（改一个字节就报错）；
2. `journal.jsonl` 的哈希链完整（被篡改过就能发现）；
3. 日志里没有未完成的调用（崩溃残留会让结果可疑）。

实测（`tools/verify_lab.sh`）：

```
$ ui lab verify /tmp/ui-lab-bundle
产物 1 个校验通过 · 日志链 完整
结论：包是自洽的

$ echo '{"accuracy":0.99}' > bundle/artifacts/metrics-1.json
$ ui lab verify /tmp/ui-lab-bundle
发现 1 个问题：
  - metrics-1.json 哈希不符（记录 5b7a44e5…，实际 5858dbe5…）
退出码: 3
```

## 输入血缘：读过的数据也留指纹

只记"谁写了产物"是不够的：数据换了版本，产物哈希依然对，但结论已经不成立。

所以 Agent 在执行每个工具前，会把 `fs.read` 目标文件的哈希写进日志（`fs/read` 事件）：

```json
{"kind":"fs/read","path":"data.csv","sha256":"f102f9ff…","bytes":12,"tool":"fs_read"}
```

打包时汇总成 `inputs.jsonl`，报告里多一张表：

```
## 读过的输入

| 文件 | 字节 | sha256 | 读取于 |
|---|---|---|---|
| data.csv | 12 | f102f9ff55c5… | seq 6 · fs_read |
```

校验时会重算这些输入（工作区不在本机则跳过，包是可移植的）：

```
$ ui lab verify bundle --workspace ./exp
产物 1 个校验通过 · 输入 1 个仍一致 · 日志链 完整

$ echo '9,9' >> ./exp/data.csv          # 数据换版本
$ ui lab verify bundle --workspace ./exp
发现 1 个问题：
  - 输入 data.csv 已改变（数据版本和当初不一样了）
退出码: 3
```

## 多 seed 对照表

```bash
ui lab table bundles/s1 bundles/s2 bundles/s3 [--json]
```

顺序是刻意的：**先验输入一致，再聚合数值**。

```
## 输入一致性
| 文件 | 一致 | 版本数 | 覆盖包数 |
| data.csv | ✅ | 1 | 3 |

## 指标
| 指标 | n | 均值 | 标准差 | 最小 | 最大 | 各次 |
| accuracy | 3 | 0.8100 | 0.0082 | 0.8000 | 0.8200 | 0.80, 0.82, 0.81 |
```

**数据版本不一致时直接返回 3**，并明确写"这张表不能求均值"——跨版本平均是最常见的学术不端。

`seed` / `id` 这类标识不会进均值（有黑名单），只聚合真正的度量。
`--json` 给出同样内容的机器可读版本，供 B 组的基准流程消费。

## 结论对账：声明的数字必须有产物背书

```bash
ui lab claim --workspace ./exp --name accuracy --value 0.72   # 显式声明
ui lab pack  --workspace ./exp --name e1 --out bundles/e1
ui lab verify bundles/e1
```

```
产物 1 个校验通过 · 输入 0 个仍一致 · 结论 1/1 对得上 · 日志链 完整   → 0
```

把产物改成 0.99 而结论还是 0.72：

```
发现 2 个问题：
  - metrics.json 哈希不符（记录 d15ad6eb…，实际 ff7ba7a1…）
  - 结论 accuracy = 0.72 在产物里找不到对应数值                   → 3
```

设计取舍：**结论必须显式声明**（`claims.json`），不从自由文本里猜数字——
猜错了比不查更糟。代价是 agent 或人要多写一行。

已知局限：旧格式的 claim（只写名字、不带路径）只能做全局匹配——产物里任意一个
数值落在容差内就算过，`seed=1` 这类巧合值仍可能误判。现在可以把结论绑定到唯一
路径（`--name 'metrics.json#/accuracy'`，数组写 `#/history/0/score`），带 `#`
的 claim 只认那一个值；旧格式仍能通过，但会打印「⚠ 未绑定路径，按全局匹配」，
`--json` 里带 `"loose": true`。只写 `#/accuracy`（不带文件名）按错误处理，不降级。

## 为什么这是壁垒

竞品的日志是"执行了什么"的摘要；我们的日志是**决策级的**（效果集 + 依据 + 批准 + 结果），
所以能推出血缘、能离线重判（`ui audit --as strict`）、能证明产物没被改过。

这三件事合起来，才是"可复现科研"的前提：
**产物可验证 + 来源可追溯 + 决策可重判**。

## 已经做完的（C 组，v1.0.1 验收）

这三项在 v1.0.1 里都已落地，并且各有门禁脚本守着：

- **输入血缘**：已实现。执行每个工具前把 `fs.read` 目标文件的哈希写进日志（`fs/read`），
  打包时汇总成 `inputs.jsonl` 并在 `ui lab verify` 时重算（见上文「输入血缘」）；
- **多 seed 聚合**：已实现。`ui lab table` 先验输入一致、再聚合数值，
  版本不一致直接返回 3（见上文「多 seed 对照表」）；
- **MCP 工具**：已实现。`mcp_call` 可调用外部 server（`ui run --mcp <config.json>`），
  调用与效果留痕（见 [11-mcp.md](11-mcp.md)）。

对应的门禁：`tools/verify_lineage.sh`、`tools/verify_table.sh`、`tools/verify_mcp.sh`（都在 `debug.sh` 里）。
