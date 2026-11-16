# 安全边界

威胁模型很简单：**模型是不可信的**。它可能被提示注入、可能自己犯错、可能被用户之外的输入影响。
所以所有约束都不能靠"提示词里写了"来成立，必须由内核强制。

## 四道边界

| 边界 | 谁在管 | 失效会怎样 |
|---|---|---|
| 效果策略 | `policy.rs`：每条效果先算决策（allow/ask/deny），`secrets/**` 无条件拒绝 | 越权读写、执行任意命令 |
| 审批精确性 | 会话规则是**字面量匹配**（`Rule.literal`） | 批准 `echo *` 后 `echo pwned` 免审 |
| 路径折叠 | `Ctx::resolve` 做词法折叠，`..` 不能离开工作区 | `fs_read ../secret.txt` 读到外面 |
| 真实路径 | `Agent::escapes_workspace`：`canonicalize` 后必须仍在工作区内 | 符号链接把工作区外的内容读进来 |
| 密钥 | `secrets/**` deny（两道：规则 + 目录不参与隔离复制） | 密钥进上下文、进日志 |

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

1. **`ui serve` 无鉴权**：只绑 `127.0.0.1`，单人使用；不要直接暴露到局域网。
2. **策略的 `?` 会匹配 `/`**：文档只说"一个字符"，实现更宽；已记录未改（改了会影响现有规则）。
3. **Windows 上的进程树与文件锁**：`kill_tree` 走 `taskkill /T /F`，锁走锁文件兜底（60s 过期），都只在 WSL 里验过 Linux 路径。
4. **多 writer 的 turn 归属只到 writer 级**：同一个 writer 内部仍是串行的，跨进程的因果顺序无法从日志推出来（只有全局 seq 定序）。

## 怎么复查

```bash
bash tools/test.sh                  # 117 个测试，其中 21 个是对抗性用例
bash tools/verify_escape.sh         # 路径穿越的 E2E 复现（应被拒绝）
bash tools/verify_pending.sh        # 退出码 + 符号链接（应被拒绝）
bash tools/verify_shell_timeout.sh  # 超时必须连后台进程一起收掉（应 < 300ms）
bash tools/verify_concurrent.sh     # 3 个进程并发写同一份日志（链必须完整）
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
| 读 | 系统文件可读（不然跑不了程序），所以 `cat secrets` 这类**读**仍然拦不住。 |
| 环境 | 靠 `envScrubbed`（另一条防线），不是 Landlock。 |
