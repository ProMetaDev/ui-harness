# 长跑（soak）

> 脚本：[tools/soak_long.sh](tools/soak_long.sh) / [tools/soak_status.sh](tools/soak_status.sh)

## 24h 运行

```bash
UI_SOAK_SECONDS=86400 \
UI_SOAK_INTERVAL=10 \
UI_SOAK_WS=/tmp/ui-soak-24h \
UI_SOAK_LOG=/tmp/ui-soak-24h.log \
UI_SOAK_STATE=/tmp/ui-soak-24h-state.json \
bash tools/soak_long.sh
```

- 每轮 `ui run --mock` + `ui verify`；
- 每轮写 state；
- 失败写 `phase=failed`，进程退出非零；
- Ctrl-C/TERM 写 `phase=interrupted`，可续跑；
- 结束写 `phase=passed`。

## 断点续跑

```bash
UI_SOAK_RESUME=1 \
UI_SOAK_SECONDS=86400 \
UI_SOAK_INTERVAL=10 \
UI_SOAK_WS=/tmp/ui-soak-24h \
UI_SOAK_LOG=/tmp/ui-soak-24h.log \
UI_SOAK_STATE=/tmp/ui-soak-24h-state.json \
bash tools/soak_long.sh
```

规则：

- 保留 workspace / journal / 轮次，不重新 `rm -rf`；
- 只把 `phase=interrupted` 或 `running` 当作可续；
- **停机时间不算已跑时间**：续跑按上次 `updatedAt - startedAt` 的活跃时长回拨；
- `phase=failed` 默认拒绝续跑；确认要续需 `UI_SOAK_RESUME_FAILED=1`；
- 二进制路径必须一致，否则拒绝续跑；
- 24h 必须是**累计活跃 24h**；如果机器关机，恢复后继续补足剩余活跃时间。

## 后台运行

不需要一直开着终端：

```bash
nohup env UI_SOAK_SECONDS=86400 ... \
  bash tools/soak_long.sh >/tmp/ui-soak-nohup.out 2>&1 &
```

但机器/WSL 本身仍要保持运行；不能靠“进程在后台”跨越关机。

## 状态检查

```bash
bash tools/soak_status.sh /tmp/ui-soak-24h-state.json
```

- `phase=passed`：0；
- `phase=failed`：2；
- `phase=interrupted`：1（可续跑）；
- `running` 但心跳超过 `max(3*interval, 300s)`：1；
- 进程还在但不再推进会被识别，不把“活着”当“健康”。

## 当前进度

- 2h soak：`phase=passed`，1429 轮，0 失败；
- 24h soak（0.3.0 基线二进制）：`phase=passed`，8580 轮，0 失败/0 超时；
  - journal 34320 事件，链完整；
  - RSS median 4540KB，max 4812KB，fd 5–8；
  - 期间经历关机与断点续跑，停机时间未计入；
  - **注意**：该次跑的是 P0/P1/P2 修复前的二进制；最终发布版仍需重跑 24h。

## 边界

- mock 模型，不是真实 provider 成本/限流；
- 只检查 journal 链、进程、基本 RSS/fd，不是完整资源剖析；
- 续跑不会补回停机时间，只补剩余活跃时长；
- 机器关机期间不计入 24h；
- 可能关机时不要把 workspace/state 放 `/tmp`：改用 `$HOME/ui-soak-...`，避免
  WSL 重启或 tmp 清理把证据删掉；
- 系统睡眠可能被计入活跃时长，跑的时候不要让机器休眠。
