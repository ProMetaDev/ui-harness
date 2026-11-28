# 会话

## 磁盘上就是一个文件

```
<ws>/.ui/journal.jsonl              ← default 会话（老路径，历史日志继续可读）
<ws>/.ui/sessions/work.jsonl        ← 名叫 work 的会话
<ws>/.ui/sessions/experiment.jsonl
```

没有数据库、没有索引：**会话 = 另一个 JSONL**。所以哈希链、`verify`、`replay`、
`audit` 这些能力**自动对每个会话生效**，不需要任何改动。

## 命令行

```bash
ui sessions                       # 列表：名字、事件数、最后活动
ui sessions --new work            # 新建
ui run --task "..." --session work
ui chat --session work
ui serve --session work
```

会话名只允许字母、数字、`-`、`_`（最长 64），并且不能命中凭据形状检测——因为它会变成文件名，
也会原样出现在列表与事件里。`../evil` 这种必须被拒。名字非法时**退回 default**，而不是拼出一个奇怪的路径；
`ui sessions --new` 则直接报错。磁盘上已存在的凭据形状文件名不会出现在 CLI 或 Web 列表里。

## 网页端

侧栏「对话」视图顶部就是会话列表（当前会话高亮，右边是事件数），下面「新建会话」。

```
GET  /api/sessions            → {current, sessions:[{name, events, mtime, current}]}
POST /api/session   {name}    → 切换（下一轮生效）
POST /api/session/new {name}  → 新建并切换
SSE  session-switched         → 页面刷新列表并清空主区
```

**切换是"下一轮生效"**：worker 在开始下一轮之前才换 journal——因为一轮跑到一半
换日志会让同一轮的记录裂成两半。

## 最容易出错的地方：历史必须跟着换

会话不只是"写到哪个文件"，**上下文也必须一起换**。否则你在 A 会话聊的内容会作为
历史进入 B 会话——那是数据泄漏。

所以 `Agent::switch_journal` 做了三件事：

1. 打开新日志；
2. 按新日志重建历史（`turn/start.prompt` → user，`model/response.content` → assistant）；
3. 替换 `history`，清掉旧会话的记忆。

Web 端使用两阶段切换：`switch_journal_with` 先验证目标日志和历史，再让 Hub 准备目标脱敏流；
全部成功后才提交 Agent 与 Hub 的会话状态。恢复失败时不会出现「Agent 已切走、Hub 仍指向旧会话」
的半切换现场，下一次任务也不会因此误走早退分支。内存快照绑定的是**原会话**的 Journal
`seq`/`hash`；Agent 因排队取消补记单独切走、无法证明与 Hub 当前日志对应时，放弃内存快照，
改为从目标日志的持久威胁状态恢复。

## 实测

```
$ ui sessions
会话                         事件         字节  最后活动
default                     4       1391  0 秒前
experiment                  4       1391  0 秒前
work                        4       1391  0 秒前

$ ui sessions --new work
ui: 会话 `work` 已经存在
$ ui sessions --new '../evil'
ui: 会话名只能用字母、数字、- 和 _（最长 64）
```

三个日志各自写入、互不干扰，三个哈希链都完整（`verify --journal <文件>`）。

Web 端：

```
新建 work → 切换 → 发消息 → 消息落在 .ui/sessions/work.jsonl
切回 default → 发消息 → 落在 .ui/journal.jsonl
非法名 → {"ok":false,"reason":"非法会话名"}
不存在 → {"ok":false,"reason":"会话不存在"}
```

## 还没做的

- **会话重命名 / 删除**（删文件就行，但没有命令）；
- **跨会话搜索**；
- **会话列表分页**（现在全量扫目录，几百个会话会慢）；
- **切换时保留草稿**（现在切走会清空输入框内容）。
