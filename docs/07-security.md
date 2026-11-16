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
