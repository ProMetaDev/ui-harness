# 33 · 对《专项评审与指导意见》（2026-11-17）的回应

> 评审对象：`ProMetaDev/ui-harness` main @ `a294c30`（v1.0.1）
> 本文档记录：我们**逐条核验的结果**、修复状态、以及**不同意的部分**。
> 原则与评审一致：能复现的才算数；修了的附测试；没修的写清楚为什么。

---

> **版本号说明（2026-11-18 追记）**：本文写于版本号还是 `v1.0.x` 的时候。
> 之后接受了"1.0 为时过早"的意见，**撤回了 `v1.0.0`–`v1.0.2` 三个 tag，改为 `0.2.0`**。
> 正文里出现的 `v1.0.x` 是当时的称谓，保留原文不改；现行规则见 [34-versioning.md](34-versioning.md)。

## 0. 总体回应

评审的四条 P0/P1 技术结论，我们**全部复现成功**，没有一条是误报。这是对我们最有价值的一次外部输入——
它指出的问题都在我们自己的测试覆盖之外，而且是"设计原则被实现细节背叛"的那一类。

我们接受"当前更接近 v0.2-alpha"的判断，并据此调整版本叙事：

- `v1.0.0` / `v1.0.1` 是**第一个可用的稳定原型**，不是"生产就绪"；
- 修复完成后发布 **v1.0.2（安全补丁）**，并在 README 显式标注"生产就绪：未达到"；
- 评审建议的"真实多文件仓库评测"列入 v1.1 的必做项。

---

## 1. 逐条核验与修复

### P0-1 敏感目录访问控制 —— 确认，全部属实

我们自己写了黑盒复现脚本 `tools/verify_secrets_isolation.sh`（跑真实二进制，看 stdout，
**不看 journal——因为泄漏在工具输出里，日志里没有正文**）：

> 后来（Phase 1-A）`tool/output` 开始进日志了，这句话的因果就变了：现在日志里
> **有**这一条，但正文先过两档处理（脱敏或按来源扣留）。这个脚本仍然只看 stdout——
> 它要复现的是"泄漏发生在输出这一层"，那一层没变。两档的语义见 docs/07-security.md。

修复前的实测输出：

```
LEAKED   fs_search(pattern='.')     → fs_search ok   secrets/astra.json:1: {"api_key":"sk-LEAK-CANARY-1234"}
LEAKED   fs_search(pattern='api')   → 同上
LEAKED   grep(path='.')             → grep ok   ./secrets/astra.json:1:{"api_key":"sk-LEAK-CANARY-1234"}
LEAKED   grep(path='secrets')       → grep ok   secrets/astra.json:1:{"api_key":"sk-LEAK-CANARY-1234"}
BLOCKED  tree(path='secrets')       （只列文件名，不含内容）
```

评审的四条判断全部成立，我们**额外发现一个细节**：泄漏发生在工具返回值/标准输出里，
`journal` 的 `tool/end` 只有 `ok/meta`，所以"查日志没看到"会误判为"没泄漏"。
这一点已写进回归脚本的注释，避免后来人重蹈。

### P0-2 截断 JSON 自动修补 —— 确认，属实

`parse_arguments` 会补引号/补括号，并在失败后**从尾部删最多 24 字符**再试，
与 `docs/02` 的"不做静默降级"直接冲突。已按评审建议改为：只保留代码围栏剥离，
解析失败一律返回 `Err`，交给既有的 `model/repair` 重采样。

### P1-1 外围组件边界 —— 确认，其中一条比评审描述更严重

`/api/tree` 的穿越我们自己复现了：

```
LEAKED   /api/tree?path=/etc      → {"path":"/etc","entries":[{"name":".java",...},{"name":"ModemManager",...}]}
LEAKED   /api/tree?path=secrets   → {"path":"secrets","entries":[{"name":"astra.json","dir":false,"size":26}]}
BLOCKED  /api/tree?path=../
```

也就是说它不只读工作区里的 `secrets/`，而是**直接读宿主机目录**。这条按 P0 处理。

`/snapshot` 未转义 `</script>`、`make_token` 用时间戳而非密码学随机源、
请求体无上限、`Args::parse` 吃子命令、`switch_journal` 丢工具结果、macOS 不 fail-closed——
逐条核验均属实，修复见下表。

### P1-2 `ui lab verify` 结论核验 —— 确认，属实且影响我们对外的可信度

`numeric_leaves` 只抽顶层标量、多文件同名键互相覆盖，`verify_bundle` 用
"任意数值落在容差内"判定。这确实可能"巧合通过"。

修复：claim 支持显式路径绑定（`metrics.json#/accuracy`、`history/0/score`），
未绑定路径时降级但**显式警告并标注 `loose: true`**。

### 工程卫生与文档 —— 确认，全部属实

`out.txt` / `sneaky.txt` 被跟踪、`tools/` 下 139 个脚本混杂一次性产物、源码中存在乱码注释、
README"三个不一样的地方"下面列了 4 条、`docs/07` 仍写"`ui serve` 无鉴权"与"117 个测试"。
这些不涉及安全，但直接损害"可审计"这个定位的可信度——一个数字都写不准的项目，说自己能审计别人，很难被信。

---

## 2. 我们对 `docs/32` 术语批评的回应

评审指出：把"观测是下界""上下文可复现≠输出可复现""无锚定单链检测不了截断"
称为"不可能性结论 / impossibility results"不严谨，它们不是 FLP/CAP 那种形式化不可能定理。

**这个批评我们接受，并已修改**：正文改称"**系统边界条件**"，并在开头加一句明确说明
"不是 FLP/CAP 式的形式化不可能定理，而是本系统与单机密码学链的既定边界"。
提交信息里那句 "3 impossibility results" 无法改（历史已推送），但文档正文不再那样表述。

保留原结论的理由：它们是**可复现的实测边界**（`tools/verify_truncation_gap.sh`、
`tools/demo_rewrite_chain.py`），不是措辞。术语要收，事实要留。

---

## 2.5 额外发现（评审没提，但我们自己撞上了）

1. **并发跑门禁会互相踩**：`tools/debug.sh` 的多个用例共用 `/tmp/ui-gate`、`/tmp/gate.out`、
   `/tmp/ui-headcheck` 与固定端口。两个 agent 同时跑，会出现"6 项失败"的假红，
   失败输出里还混着另一个 run 的行。→ 规则：**同一时间只允许一个 debug.sh**。
   这条已写进 `docs/31` 的工程规则。
2. **Windows 随机源的验证边界**：`BCryptGenRandom` 分支在 Linux 上无法执行，
   只有交叉编译的类型检查。子 agent 曾在 Windows 主机上跑过交叉编译出来的测试 exe（7 passed），
   但那是 gnu target、不是 MSVC，我们不把它写成"已验证"。
3. **`tools/chat_compat.mjs` 有同类静默降级**：envelope 解析失败时把原文当 answer、
   `args` 非对象时静默换成 `{}`。已按 `deepseek.mjs` 的正确做法修掉。
   它目前没有自动化门禁覆盖——这是本次的遗留项。

## 3. 修复状态

| 编号 | 问题 | 状态 | 证据（测试名 / 脚本） |
|---|---|---|---|
| P0-1 | secrets 四条读取路径 | **已修** | `tools/verify_secrets_isolation.sh`（6 条全 BLOCKED）；`kit.sh` 场景 9–12；`tools::tests::every_read_entry_skips_protected_dirs`；`policy::tests::secrets_directory_itself_is_denied`；`adversarial::fs_mv_checks_every_fs_effect_for_symlink_escape` |
| P0-2 | 截断 JSON 静默修补 | **已修** | `model::tests::truncated_arguments_are_an_error_not_salvaged`；`strict_arguments::truncated_fs_write_arguments_are_repaired_not_written`（断言 repair 事件在任何 `tool/start` 之前） |
| P1-1a | `/api/tree` 穿越（含宿主机目录） | **已修** | `tools/verify_api_traversal.sh`（`/etc`、`secrets`、`../` 全 BLOCKED）；`server::tests::tree_rejects_absolute_and_secret_paths`、`tree_rejects_symlink_escape`、`file_preview_uses_the_same_path_guard` |
| P1-1b | `/snapshot` 未转义 `</script>` | **已修** | `server::tests::snapshot_escapes_script_close_in_events` |
| P1-1c | 令牌熵源弱 | **已修**（Unix 实测；Windows **未实机验证**） | `server::tests::make_token_is_hex_from_os_randomness`、`token_entropy_failure_is_hard`、`urandom_is_the_entropy_source` |
| P1-1d | 请求体无上限 | **已修** | `server::tests::oversized_body_is_rejected_with_413`（4 MiB 上限） |
| P1-1e | `Args::parse` 顺序敏感 | **已修** | `cli::bool_switch_before_subcommand_does_not_swallow_it`、`all_boolean_switches_keep_a_following_subcommand` |
| P1-1f | `switch_journal` 丢工具结果 | **已修** | 新增 `tool/output` 事件（记录模型实际看到的工具输出，含截断指针）；切换会话时还原为 tool 消息 |
| P1-1g | macOS 未 fail-closed | **已修** | 非 Linux/Windows 且 `sandbox.fs` 时 `shell` 直接拒绝执行，需显式 `UI_SANDBOX=0` |
| P1-2 | `lab verify` 巧合通过 | **已修** | claim 绑定路径（`a.json#/accuracy`、数组下标）；未绑定 → `loose: true` + 警告；只写 `#/x` → 硬错误。7 个单测 + 4 个 CLI 端到端测试 |
| 卫生 | `out.txt`/`sneaky.txt`/乱码/一次性脚本 | **已修** | 删除 2 个遗留文件 + 8 个零引用脚本；`.gitignore` 补规则；乱码注释修复；新增 `tools/README.md` |
| 文档 | 数字与状态失步 | **已修** | README/SECURITY/docs 02·07·09·31·32 同步；硬数字改为"以脚本输出为准" |

**最终数字**（v1.0.2，独占跑、无并发）：

```
bash tools/test.sh    → 224 passed / 0 failed / 1 ignored
bash tools/debug.sh   → 50 passed / 0 failed
bash tools/kit.sh     → 12 个攻击场景，9 BLOCKED / 1 REPAIRED…（见脚本输出）
```



---

## 3.5 一次没能复现的 CI 偶发失败（如实记录）

修完这批问题后第一次推 CI，`linux` job 在 "unit + integration tests" 挂了一个：
注解显示某个 20 用例的测试二进制 **19 passed / 1 failed**（即 `ui-cli/tests/cli.rs`），
但——

- 本地全量跑：20/20 通过；
- 串行跑（`--test-threads=1`）：20/20 通过；
- 用最小环境（`env -i`、新 `HOME`/`TMPDIR`、`CI=true`）：20/20 通过；
- 同一二进制连跑 5 次：全过。

随后**只改了 `tools/ci.sh` 的诊断输出**再推，CI 转绿。所以这是**一次没能复现的偶发失败**，不是确定性缺陷。

我们没有把它当"过去了"：
1. `tools/ci.sh` 现在失败时会把 `test ... FAILED` / `panicked at` 单独打印（annotations 不需要鉴权即可读，CI 日志需要）；
2. 下次再出现，能直接看到是哪个测试，而不是只知道"20 个里挂了 1 个"。

**这一条不算已解决，只算已埋点。** 如果它再次出现，会按"真实缺陷"处理。

---

## 4. 我们不同意的两处

### 4.1 "建议暂缓使用 1.0 版本定位" —— 部分接受

把版本号当"成熟度承诺"是对的，但 1.0 在我们这里定义的是 **API/契约稳定**，不是"生产就绪"。
强行把已发布的 tag 撤回会造成更大的混乱（已有人可能引用 `v1.0.0`）。

我们的做法：**保留 tag，但在 README 顶部明确写"生产就绪：未达到（见 docs/33）"**，
并在 CHANGELOG 里把 v1.0.2 标为安全补丁。这比"把 1.0 撤回去"更诚实，也更有用。

### 4.2 "升级到原生 Function Calling" —— 同意方向，但要先算账

评审说得对：纯文本拼接 + 正文双重转义 JSON 会提高截断概率，也吃不到 Prefix Caching。
这是真实代价。但它牵动的是**整个 `Snapshot` 契约**（一步一快照、分段计价、确定性哈希）：

- 现在的快照是"一条字符串 + 分段统计"，换成 `messages + tools` 之后，
  分段计价要改成"对结构化请求对象逐段哈希"，等价性需要重新证明；
- 我们的 `PromptSnapshot` 哈希是审计证据的一部分（`prompt/snapshot` 事件），
  协议一换，历史日志的可比性就断了。

所以这条我们排在**第三阶段**，并且要求：迁移期间**双协议并存**，旧日志仍可解释。
评审把它列为"第三阶段"是对的，我们只是把理由写清楚。

---

## 5. 我们补做的一件事：把评审的发现变成门禁

外部评审的价值在于"它发现了我们自己测不出来的东西"。所以每一条 P0 修复都配一个**黑盒脚本**进 `tools/debug.sh`：

- `verify_secrets_isolation.sh`：四条读取路径，看 stdout 判泄漏；
- `verify_api_traversal.sh`：起服务、curl `/api/tree`，断言读不到 `/etc` 与 `secrets/`。

这样下次有人改坏，门禁会红，而不是等下一份评审。
