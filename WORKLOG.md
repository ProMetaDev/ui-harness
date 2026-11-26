# Worklog

## 2026-11-26 · 二轮评审 P1/P2 并行收口

- `feat/protected-search` / `f92d3c6`：grep / literal_search / fs_search / tree
  与写入口统一过滤敏感名；grep 结果行按所有候选路径 fail closed。
- `feat/admin-consistency` / `28006f5`：管理面专用写锁；intent → persist →
  final audit；owner 不变量；lab report/run、token 常量比较、viewer metrics。
- `feat/worker-hardening` / `fe3b3ba`：claim JSON、session/workspace registry、
  workspace lease、lease_lost 终止、psql stdin 批量同步；live 6.2MB 事件通过。
- `feat/p2-cleanup` / `160dbf2`：BOOL_FLAGS 补 6 项；`checkpoint` 因双语义保留
  为例外并加防遗漏测试；WORM `--now` 仅 `UI_WORM_TEST_CLOCK=1`；乱码注释、
  README/CHANGELOG/docs 同步。
- 集成：`tools/debug.sh` 57/57；Rust 全量；Postgres live/worker；runner live；
  OIDC e2e 20；WORM 38；audit→WORM 33。
- 24h soak 未受代码合并影响，继续跑；但仍是旧二进制证据，最终版需重跑。

残余（不假装完成）：

- 硬链接别名仍无法按路径识别；
- `checkpoint` 严格 BOOL_FLAGS 需要 command-aware 解析；
- docs 32/33 重号未改名；
- 4 个 soak 脚本未合并公共层；
- 真实 IdP/付费 provider/S3 Object Lock/24h 最终版未完成。

## 2026-11-26 · 二轮评审 P0 并行修复

按老师评审表 2 的三条 P0 并行，各自 worktree，互不抢文件：

- N-P0-1 `feat/protected-read` / `fb351ba`：
  - `is_allowed_read` 增加真实路径 canonicalize，软链不能借 `.ui/tool-output/` 例外；
  - `.env*`/私钥/证书名接入 fs_read、`/api/file`、`/api/tree`；
  - 回归：软链到 secrets/工作区外、敏感名别名均拒绝。
- N-P0-2 `feat/runner-hardening` / `4b75373`：
  - 容器匿名卷遮蔽 `secrets/.ui/.git`，根 `.env` bind `/dev/null`；
  - 拒绝负数/缺失 Content-Length、chunked；执行期 8MiB 输出上限；
  - live 测试新增 mask/body-validation/output-cap。
- N-P0-3 `feat/oidc-hardening` / `d7728db`：
  - aud 默认 strict；`OIDC_IDENTITY=sub|email`，email 需 `email_verified`；
  - SSE 独立超时；非回环强制 HTTPS；Host 统一小写；
  - 27 项单测 + 20 项 OIDC e2e 通过。

集成后：

- `tools/debug.sh` **57 passed / 0 failed**；
- Rust 全量通过（一次 `lab_claims` 偶发失败，单跑与连续 3 次全量通过）；
- `verify_runner_live.sh`：mask/body-validation/output-cap 通过；
- `verify_secrets_isolation.sh`：6/6 BLOCKED；
- OIDC 单测 27、e2e 20；
- 24h soak 未受合并影响，仍在跑。

残余：

- `grep`/`fs_search` 输出与 `fs_edit` 尚未按敏感名过滤；
- N-P1-1 管理面审计顺序/写锁、N-P1-2 Worker session/lease、N-P1-3 Web 细节、P2 未做。

## 2026-11-24 · 版本 0.3.0 + soak 断点续跑

- 版本号 `0.2.2 → 0.3.0`，未打 tag；tag 等 24h 与真实外部验证后。
- `tools/soak_long.sh` 支持 `UI_SOAK_RESUME=1`：
  - 保留 workspace/journal/轮次；
  - 停机时间不计入已跑时间，只补剩余活跃时长；
  - Ctrl-C/TERM 记 `phase=interrupted`，可直接续；
  - `phase=failed` 默认拒绝续，需 `UI_SOAK_RESUME_FAILED=1`；
  - 二进制路径变化拒绝续跑。
- 实测：10s soak 中断后从 round=4 续到 round=7，`phase=passed`。

## 2026-11-24 · 第三轮并行：OIDC e2e / 导出到 WORM / 付费 smoke

- OIDC e2e（`1534b14` + wrapper `38e95a2`）：mock IdP → oidc_proxy → ui serve →
  `/api/admin` + 浏览器 `/admin`；20 项通过，含伪造头剥离、401/403、审计、
  日志不含 token/代理密钥。
- 导出到 WORM（`1c96476`）：`audit_export_to_worm.py` 串起
  export → OpenSSH 签名 → anchor → WORM put → verify；33 checks。
- 付费 smoke（`31f8439`）：`soak_paid_smoke.sh` 固定 1 轮、四道闸、预算
  0.01 美元、`smoke-report.json`；scripted e2e 通过，真实付费 provider 未实跑。
- 集成验证：`tools/debug.sh` **57 passed / 0 failed**；docs 79/79；
  独立性 PASS；scan_secrets PASS。

## 2026-11-24 · 第二轮并行：可写管理台 / WORM / 付费长跑

- M6（`6b1d3ae`）：`.ui/users.json` 用户存储（禁用用户全路径 403、fallback
  `.ui/rbac.json#users`）、可写角色矩阵/用户表单、`/api/admin/users`、OIDC
  userinfo 适配层（Bearer → 剥离客户端头 → 注入可信头）；15 项 OIDC mock 测试。
- WORM（`a8bf1b5`）：`adapters/worm` 本地不可变对象库、retention/tombstone、
  append-only index；37 项门禁。边界：不是 S3 Object Lock。
- 付费长跑（`8978e62`）：`tools/soak_paid.sh` 四道闸、预算/token 上限、
  `paid-report.json`；真实付费 provider 未实跑，scripted e2e 通过。
- 集成修复（`80f92a9`）：管理台表单纳入真实浏览器 e2e（`forms=ok`）；
  `soak_real.sh` 失败路径 `printf --` 修复，失败会正确写 `phase=failed`。
- 验证：`tools/debug.sh` **57 passed / 0 failed**；docs 79/79；独立性 PASS；
  OIDC 15 tests；WORM 37 checks；audit sign 27 checks；paid/real soak 默认 SKIP。

## 2026-11-24 · 三路并行：M6 / 非对称签名 / 真实长跑

- M6 RBAC（`c5cf606`）：`.ui/rbac.json` 配置驱动角色矩阵 + 用户映射；
  `GET /api/admin/rbac`，`PUT /api/admin/roles`（owner），`PUT/DELETE /api/admin/users`（manage）；
  写 `rbac/role-update` / `rbac/user-update`；管理页只读表。
- 安全复核补丁（`e156d85`）：admin 的 manage 不能授予/撤销 owner，只有 owner 可以。
- 非对称签名（`83e4b3e`）：`adapters/audit/sign_export.py` 用 OpenSSH sign/verify；
  `anchor.py` 追加锚定链；`tools/verify_audit_sign.sh` 27 checks。
- 真实长跑（`9a7cf27`）：`tools/soak_real.sh` 默认 SKIP，需 `UI_MODEL_CMD` +
  `UI_SOAK_ALLOW_COST=1`；每轮真实 run + verify + 工具证据检查。
- 集成门禁：`tools/debug.sh` **57 passed / 0 failed**；docs 79/79；独立性 PASS。
- 边界：真实付费 provider 未实际调用（环境无可用 provider）；M6 页面仍只读；
  签名是 OpenSSH 非对称 + 锚定，不是 WORM；24h 未跑。

## 2026-11-23 · 多智能体第一轮集成

三个独立 worktree 并行，主 worktree 不参与实现；合并顺序 security → data → verify。

- `feat/security-runner-token` / `fc53795`：runner 无 token 拒绝启动；受信代理加
  `--trusted-proxy-secret`，无密钥 fail closed；密钥不进事件/journal。
- `feat/pg-migrations` / `a65f077`：幂等迁移 + `ui_schema_migrations`、任务优先级、
  queued/running cancel、worker 取消路径；真实 Docker Postgres live 通过。
- `feat/soak-metrics` / `ef373b0`：soak 改采 ui 子进程 RSS/fd、失败/超时记录；
  定位 lab_pack flake 根因是 mtime vs wall-clock。
- 集成后补修：
  - `.gitattributes` 强制 `*.sh/*.py` LF；
  - `lab_pack` 用 journal `fs/read` path+sha 判定输入，mtime 回跳不再把输入误判成产物；
  - 回归测试把输入 mtime 推到未来。
- `tools/debug.sh`：**57 passed / 0 failed**。
- **2h soak（新脚本）通过**：`phase=passed`、rounds=1429、failedRounds=0、
  journalChainOk=True；RSS 中位数 4484KB、max 4840KB、fd 5–8；
  first50 avg 4219KB / last50 avg 4495KB（+276KB，未证明泄漏，24h 继续看趋势）。
- 旧 2h soak 在 1405/1440 轮后因旧脚本进程检查退出 1；不是产品失败，也没有 `phase=passed`。
- checkpoint GC：`ui checkpoints --gc --keep N`，默认 dry-run；只保留被保留
  manifest 引用的 blob，清理 `.tmp-*`。

## 2026-11-23 · 企业硬化与审阅闭环

### 目标

把 UI-Harness 从“本地单用户 Agent Harness”推进到“可验证的企业单租户基础”：

1. 干净安装 / Web e2e / 长跑；
2. M1 受信代理 + RBAC；
3. Postgres 事件/任务/审批 adapter；
4. 增量代码索引；
5. Web review 历史 + 单文件回退；
6. 企业 M2–M6。

### 已完成

- **变更审阅**
  - checkpoint / diff / hunk review / restore；
  - 单文件 revert；
  - 自动 checkpoint（每个 turn 首次写操作前）；
  - review/revert 写 journal；
  - Web 审阅面板、历史、基线提示、单文件回退按钮。

- **代码索引**
  - `ui index` / `ui index --incremental`；
  - 文件/符号/trigram 风格路径匹配；
  - prompt 注入“相关文件地图”；
  - size + mtime 复用未变文件。

- **M1**
  - `ui serve --trusted-proxy loopback`；
  - `X-UI-User/UserId/Roles`；
  - 无 actor 401、viewer 写 403；
  - review/revert/approval 记录 actor。

- **Postgres adapter（L2）**
  - `ui_events` append-only + 触发器；
  - `ui_task_records` / `ui_approval_records` 物化；
  - `ui_claim_task` / `ui_heartbeat_task`；
  - 真实 `postgres:16-alpine` live 测试；
  - 最小 worker：claim → 真实 `ui run` → heartbeat → done/failed；
  - `--sync-events` 把 journal 增量回写。

- **M3 runner（L2）**
  - Docker 容器执行；
  - workspace-only 挂载、rootfs read-only、无网络、资源上限；
  - 超时 kill、输出上限；
  - live 测试覆盖 workspace/network/timeout/token/escape。

- **M4/M5/M6 基础**
  - `/api/metrics`；
  - `/api/audit/export`（含 seq/limit）；
  - 只读 `/admin`；
  - 浏览器 e2e 覆盖聊天页 + `/admin`。

- **验证**
  - 干净 target 构建 + doctor；
  - `lab_pack` flake 循环 20/20；
  - 真实浏览器 CDP e2e；
  - 2h soak 运行中（5s 一轮）。

### 关键修正

- **隐私边界回归**：自动 checkpoint 曾可能把 `.env`/`secrets/` 复制进 `.ui/checkpoints/blobs`。
  - 已修：checkpoint/index 统一跳过 `.env*`、私钥/证书、`secrets/`、`--private` 路径。
  - 测试：`checkpoint_skips_secret_files_and_private_dirs`、`index_skips_secret_files_and_private_dirs`。

## 2026-11-23 · soak 指标修复与 lab_pack flake 取证（worktree `ui-agent-verify` / `feat/soak-metrics`）

### 改动

- `tools/soak_long.sh`：不再用 `ps -o rss= -p $$` 采本 shell 的 RSS。
  - `/usr/bin/time -v` 可用时取 `Maximum resident set size`（RSS 权威来源）；
  - 否则 spawn 后轮询被测 ui 进程树：`/proc/<pid>/status` 的 `VmRSS`、`/proc/<pid>/fd` 计数；
    无 `/proc`（macOS）降级到 `ps -o rss=` + `lsof`；仍然拿不到就写 `na`，不编造。
  - 每轮日志、`$STATE` 的 `metrics`/`lastRound`、`$STATE.rounds.jsonl` 都写清
    `rssSource`/`fdSource`；`runnerShellPid` 只作对照，shell RSS 不再进入指标。
  - 每轮 run/verify 分别记录 max RSS、fd；新增单命令超时
    `UI_SOAK_ROUND_TIMEOUT`（默认 600s）。失败/超时写
    `RESULT ... status=fail stage=...`、state 的 `lastError`/`failedRounds`，state 仍是 tmp+mv 原子写。
  - 顺带修了 journal 链检查：以前 grep 整份累计日志，可能被上一轮的“哈希链完整”蒙混；
    现在只看本轮 verify 的输出。
- `tools/soak_status.sh`：展示指标来源、`lastRound` 的 run/verify 明细与 `lastError`；
  `phase=running` 时校验 `runnerPid` 是否真活着；旧 state 没有 `schemaVersion` 时明确警告
  “指标来源未知，旧脚本可能采的是 shell RSS”。
- `tools/verify_flake.sh`：默认 20 次单测 + 3 次 `cargo test --workspace`；失败时保留日志、
  `/tmp/ui-cli-*-labpack`（含 journal/inputs.jsonl/manifest），自动做“并行 vs 串行”对照与
  mtime/journal 取证；没有修改或放宽任何断言。

### 验证（WSL Ubuntu-22.04，2026-11-23 21:44–22:03）

- `UI_SOAK_SECONDS=10 UI_SOAK_INTERVAL=1 bash tools/soak_long.sh`：PASS `rounds=8`；
  ui run 的 RSS 3724–4280KB（`gnu-time-v`）、fdMax=8（`proc-fd-tree`）。
  同一轮的 shell RSS 约 3484KB，已经只作为 `runnerShellPid` 对照，不再冒充 ui 指标。
- `bash tools/soak_status.sh /tmp/ui-soak-long-state.json`：exit 0，`phase=passed rounds=8`，
  来源显示为 `rssSource=gnu-time-v fdSource=proc-fd-tree`。
- 降级/失败路径：`UI_SOAK_METRIC=proc`、`=ps` 各跑 3 秒均通过（分别打出
  `rssSource=proc-tree-rss` / `ps-tree-rss`、`fdSource=proc-fd-tree` / `lsof-fd-tree`）。
  用 `/tmp` 假 ui（`sleep 5; exit 7`）+ `UI_SOAK_ROUND_TIMEOUT=1` 验证超时：
  日志/state 明确 `rc=124 timedOut=1`、`phase=failed`、`lastError=round=1 stage=run ...`，
  超时进程树被清理。说明：ui verify 常只有十几毫秒，短于采样窗口时 `fdMax=0 fdSource=na`
  表示“没采到”，不伪造；ui run 的 RSS/fd 是验收主指标。
- `bash tools/verify_flake.sh`：单测 20/20、全量 `cargo test --workspace` 3/3 全绿
  （单测约 1.8s/次，全量约 30–36s/次）。连续两轮 20+3 都没有复现 flake；第二轮是在
  机器 loadavg≈10、其他任务并发时跑的，仍然全绿，进一步说明不是并行本身导致。
- flake 取证（历史残留 + 机制复现）：
  - 在 `/tmp` 里找到一次真实失败现场 `ui-cli-62356-labpack`：`manifest.jsonl` 有 3 条
    （`artifacts/metrics.json` + `fixtures_input.txt` + `sc.json`），`inputs.jsonl` 为空，
    正好会触发该测试的两条断言（“产物 1 个”“输入要记下来”）。
  - 对比 45 个 labpack workspace：正常 run 的 `sc.json`/`fixtures_input.txt` mtime 比
    journal 首事件早 5–117ms；该失败现场偏偏晚 400ms。
  - 用 `/tmp` 辅助脚本做对照：mtime 正常时 manifest=1 且 inputs 含 fixtures；
    把这两个文件 mtime 手动 +1.5s 后，`lab pack` 得到 manifest=3、inputs 为空。
    机制确认。
  - 结论：不是并行竞争，也不是自动 checkpoint 引入；根因是 `lab_pack` 用 wall-clock
    的 `run_start` 去比较文件 mtime，WSL2 墙钟回跳（`docs/28-isolate-copy.md` 已记录该现象）
    时，把 run 之前就存在的输入文件误判成“本轮产物”。`lab verify` 对 3 个产物仍报“包自洽”，
    所以测试里的“产物 1 个”断言才是拦截点。
  - 产品侧修复不在本 worktree 范围；建议后续改用 journal/checkpoint 边界或单调时钟判断
    “本轮新写完的文件”，不要直接比 wall-clock ms。

### 当前问题

- **24h soak 未完成**：`/tmp/ui-soak-24h-state.json` 停在 `rounds=19`、`updatedAt=20:20`；
  `soak_status.sh` 判心跳过期（exit 1），`phase` 仍是 `running`（旧脚本没有 finished 语义）。
- **当前 2h soak 在跑**：20:21 起 7200s、5s 一轮；22:03 检查时 `phase=running`、
  `rounds=1196`、心跳 age=4s（未杀，未改其 state/日志）。预计 22:21 结束。
- ~~trusted-proxy loopback 可被本机进程伪造~~：已加 `--trusted-proxy-secret`，
  无密钥启动 fail closed；仍需生产环境用 Unix socket/mTLS 进一步收口。
- ~~runner 默认 token 为空~~：runner 现在无 token 启动即退出。
- ~~`lab_pack` flake~~：已修：用 journal 的 `fs/read` path+sha 识别输入，
  mtime 扫描不再把未变输入升级成产物；回归测试把输入 mtime 推到未来。
- ~~checkpoint blob 没有 GC~~：已加 `ui checkpoints --gc --keep N`，默认 dry-run。
- ~~review 是“先写文件再写 journal”~~：已改成先写 `review/intent`，再 apply/error；
  `--pending` / `--reconcile` 可对账 dangling intent；`unknown` 不自动改盘。
- 审计导出已支持可选 HMAC-SHA256（`UI_AUDIT_HMAC_KEY`），但 WORM/非对称签名未做。
- M6 用户/角色/策略管理、合规试点未做。

### 下一步

1. M6 用户/角色/策略管理；
2. RC 阶段再跑 24h。

### 常用命令

```bash
cargo test --workspace
bash tools/verify_docs.sh
bash tools/verify_independence.sh
bash tools/verify_flake.sh
bash tools/verify_postgres_live.sh
bash tools/verify_postgres_worker.sh
bash tools/verify_runner_live.sh
python tools/browser_e2e.py --ui out/ui.exe
bash tools/soak_status.sh /tmp/ui-soak-2h-state.json
```
