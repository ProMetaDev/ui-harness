# 跟着敲一遍：看一次真正的审计

不用模型、不用网络、不花钱。全程 30 秒：

```bash
bash tools/demo-audit.sh
```

下面是它**真实跑出来的输出**（不是我手写的）。每一步后面写了"这一步在证明什么"。

---

## 1. 跑一轮

```console
$ ui run --task demo --mock sc.json --workspace /tmp/ui-demo-audit --yes
模型 用 shell 生成报告
  → shell     ok   (无输出)
模型 再精确写一个配置
  → fs_write  ok   写入 10 字节到 /tmp/ui-demo-audit/config.json
模型 完成
结束原因 done · 2 步 · 2 次工具 · 约 2214 tokens
```

这里刻意混用了两类工具：

- `shell` 写文件——**只声明"要执行这条命令"**，内部行为不在声明里；
- `fs_write`——**精确声明了路径**。

> 证明：两种通道都能跑，后面的差别才有意义。

## 2. 回放

```console
$ ui replay --workspace /tmp/ui-demo-audit
16 条事件 · /tmp/ui-demo-audit/.ui/journal.jsonl
   4 14:30:57 tool/request       shell · ask · 执行命令
   5 14:30:57 approval/answered  shell · allow
   6 14:30:57 tool/start         shell · 有副作用
   7 14:30:57 tool/end           shell · ok
   8 14:30:57 effect/observed    shell · 实际改动 +1 ~0 -0 · 例 report.txt
  11 14:30:57 tool/request       fs_write · allow · 匹配规则
  13 14:30:57 tool/end           fs_write · ok
```

注意第 8 行：**shell 到底改了什么，日志里有**（`report.txt`）。
再注意第 11 行后面**没有** `effect/observed`——`fs_write` 已经声明了路径，不需要再观测一遍。

> 证明：声明覆盖不到的通道，"实际改动"由内核观测补上；声明得清的通道不重复记。

## 3. 校验

```console
$ ui verify --workspace /tmp/ui-demo-audit
事件  16
序号  1..16
轮次 1  工具调用 2
未完成 turn=false tools=0
退出码: 0
```

> 证明：链是完整的，没有半途而废的调用。

## 4. 篡改一个字符

```console
$ sed -i 's/acc=0.96/acc=0.99/' /tmp/ui-demo-audit/.ui/journal.jsonl
```

把报告里的数字从 `0.96` 改成 `0.99`——**这正是"事后美化数据"的样子**。
这不是假想：实验跑砸了改个数字，是最常见的学术不端形态。

## 5. 发现

```console
$ ui verify --workspace /tmp/ui-demo-audit
FAIL  日志损坏 seq=Some(3): hash mismatch
      文件 /tmp/ui-demo-audit/.ui/journal.jsonl
ui verify 退出码: 2

$ python3 tools/verify_journal.py /tmp/ui-demo-audit/.ui/journal.jsonl
  第 3 行哈希不符：记录 24dbae365e6f… 实际 de0f9a3846e6…
python 退出码: 2
```

**两套独立实现都抓到了**：Rust 内核的 `ui verify`，和 60 行 Python 的
`tools/verify_journal.py`（它不依赖 Rust 代码，只依赖
[格式说明](22-verify-kit.md)）。

> 证明：校验不需要相信我们的实现。你可以用任何语言在 20 行里复现它。

## 6. 对照：截断可修，"被改"不可修

```console
$ python3 tools/verify_journal.py <被截断的日志>
  第 16 行不是完整记录（长度 230）
记录 15 条 · 链尾部残缺
  尾部有 1 行不完整（崩溃残留，可修复）
python 退出码: 3
```

退出码 **3 而不是 2**：这是"崩溃撕掉半条"，不是"有人动了手脚"。
再跑一轮时，打开日志会截掉撕裂的尾巴，**并且把"修过"这件事记下来**：

```console
$ ui replay ... | grep torn
  16 14:30:53 journal/torn       崩溃留下的半条记录已截断
```

> 证明：修复不是偷偷摸摸的——它自己也是一条日志。

---

## 这三个数字要记住

| 退出码 | 含义 | 能不能继续用 |
|---|---|---|
| `0` | 链完整 | 可以 |
| `2` | **被改过**（哈希不符） | 不可以，得查 |
| `3` | 尾部残缺（崩溃残留） | 可以修复 |

## 接下来可以试

```bash
bash tools/kit.sh                        # 7 个攻击场景：哪些拦得住、哪些拦不住
ui audit --as strict --workspace WS      # 用严格模式重判历史
ui policy --effect fs.write --target /etc/passwd   # 解释一条效果为什么被拒
```

想跑真模型：见 [README](../README.md) 的「30 秒上手」；
想看成套实验链路：`tools/lab_real.sh`（3 个 seed，产物哈希 + 结论对账）。
