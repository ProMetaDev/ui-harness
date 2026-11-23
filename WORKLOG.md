# Worklog

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

### 当前问题

- 24h soak 未完成；当前跑的是 2h soak。
- `trusted-proxy loopback` 仍可能被本机进程伪造身份，生产需 Unix socket/mTLS/UID 校验。
- runner 默认 token 为空时必须拒绝启动；目前只靠绑定 localhost。
- `soak_long.sh` 的 RSS 采样是 shell，不是 `ui` 进程，不能证明 ui 无泄漏。
- checkpoint blob 没有 GC/保留期。
- review 是“先写文件再写 journal”，中间崩溃会留下未审计修改。
- `lab_pack` flake 根因未定位。
- M6 用户/角色/策略管理、审计 WORM、合规试点未做。

### 下一步

1. 完成 2h soak 并看 `phase=passed`；
2. 修 trusted-proxy 信任与 runner token 默认值；
3. checkpoint blob GC；
4. review 事务化/补偿审计；
5. RC 阶段再跑 24h。

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
