# 长跑（soak）

> 脚本：[tools/soak_long.sh](tools/soak_long.sh) / [tools/soak_status.sh](tools/soak_status.sh)

## 24h 运行

```bash
UI_SOAK_SECONDS=86400 \
UI_SOAK_INTERVAL=60 \
UI_SOAK_WS=/tmp/ui-soak-24h \
UI_SOAK_LOG=/tmp/ui-soak-24h.log \
UI_SOAK_STATE=/tmp/ui-soak-24h-state.json \
bash tools/soak_long.sh
```

- 每轮 `ui run --mock` + `ui verify`；
- 每轮写 state：

```json
{"phase":"running","rounds":1,"startedAt":...,"updatedAt":...,"seconds":86400,"interval":60}
```

- 失败写 `phase=failed`，进程退出非零；
- 结束写 `phase=passed`。

## 状态检查

```bash
bash tools/soak_status.sh /tmp/ui-soak-24h-state.json
```

- `phase=passed`：0；
- `phase=failed`：2；
- `running` 但心跳超过 `max(3*interval, 300s)`：1；
- 进程还在但不再推进会被识别，不把“活着”当“健康”。

## 当前进度

24h soak 已启动，**尚未结束**；在 `phase=passed` 前不能宣称第 1 条完成。

## 边界

- mock 模型，不是真实 provider 成本/限流；
- 只检查 journal 链、进程、基本 RSS/fd，不是完整资源剖析；
- 没有自动重启/断点续跑。
