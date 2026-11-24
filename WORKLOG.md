# Worklog

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
