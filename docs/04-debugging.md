# 标准调试方式

一句话：**把故障压到不需要终端、不需要模型的那一层再修**。
越往下越可复现；越往上越要靠眼睛。

四层，每层都能独立复现问题：

| 层 | 复现方式 | 看什么 | 典型故障 |
|---|---|---|---|
| **L0 看一眼** | `ui export --html out/trace.html` 然后双击 | 每步的快照、决策、参数、账本 | 先确认"我以为是问题的地方"真的是问题 |
| L1 内核 | `cargo test`、`ui verify`、`ui replay --json` | 事件序列、决策、账本 | 预算没扣、策略放行错了、日志断裂 |
| L2 帧 | `ui preview --cols N --rows M`、`cargo test frame::` | 每行恰好 N 列、标签对齐 | 中文把右边框顶歪、换行吞字 |
| L3 终端 | `bash tools/tui_smoke.sh`（`script` 造伪 tty） | 进出备用屏、按键、退出还原 | 按键丢、光标乱、终端没还原 |
| L4 真机 | `bash tools/run-tui.sh` | 手感、颜色、滚动 | 只有真人能判断的东西 |

**L0 是给人和给人之外的**：导出的 HTML 自带数据、无外链，可以贴进 issue、发给别人，
不需要装 cargo、不需要 WSL、不需要终端。定位问题的第一步永远是"先把现场摊开"。

## 一条命令跑完 L1–L3

```bash
bash tools/debug.sh
```

任何一层 FAIL 就停在那层，别往上看——上层的问题往往是下层带出来的。

## 标准流程（修 bug 时照做）

1. **先写一个失败的测试**，放在最低能复现的层。修不动的东西，多半是还没来得及断言的东西。
2. 修到测试变绿。
3. `bash tools/debug.sh` 全绿。
4. 只有 L1–L3 绿了，才进 `run-tui.sh` 看手感。
5. 手感问题如果能量化（对不齐、错位、闪烁），照样回 L2 补一个帧测试。

## 常用命令

```bash
# L0：把这一轮摊开成网页，双击看（不需要终端、不需要编译）
ui export --workspace demo --html out/trace.html

# 校验导出是否完整（内嵌 JSON 能解析、无外链）
node tools/check_viewer.mjs out/trace.html

# 日志自检：链是否完整、有没有崩溃留下的未完成调用
ui verify --workspace demo

# 机器可读的事件流（配合 jq 查"第几步花了多少"）
ui replay --workspace demo --json | jq -c 'select(.kind=="tool/request") | {tool,decision}'

# 不进终端看界面（含颜色）
ui preview --cols 84 --rows 26 --ansi | less -R

# 伪 tty 端到端：进屏、画帧、审批键、退出
bash tools/tui_smoke.sh

# 把录下来的 ANSI 流还原成静态屏幕（没有终端也能审界面）
node tools/replay_screen.mjs /tmp/tui1.out 3
```

没有终端时的兜底：`stty sane`。`ui tui` 的 `Drop` 已经会做，崩溃路径也覆盖了。

## 三个真实案例（模板）

**换行被当成字符。** 症状：用户消息后半句跑到前一句后面。
定位：L2——`ui preview` 就能复现，与终端无关。根因：`wrap_cols` 把 `\n` 当 1 列宽字符。
固化：`wraps_on_explicit_newlines`。

**按键丢失。** 症状：按 `yq` 放行后界面卡住。
定位：L3——只有 pty 里才出现。根因：一次 `read()` 带回两个键，只取了第一个。
固化：`parse_keys` 的解析测试 + 按键队列（`Tui::pending`）。

**浮层边框错。** 症状：底边是 `└────└`。
定位：L3（截图里一眼看出），但根因在 L2 的拼字符串。根因：左右角用了同一个字符。
固化：`approval_overlay_is_drawn_and_stays_inside_the_frame` 现在会检查矩形完整性。

## 不要做的事

- 不要靠"再跑一次看看"来定位：`ScriptedModel` 是确定性的，同样的剧本必须每次一样。
- 不要在 `frame.rs` 里读时间、读终端尺寸、读环境——那会让 L2 失去可复现性。时间由 `ScreenState` 传入。
- 不要在没有失败测试的情况下改渲染：宽度纪律（每行恰好 N 列）极容易被一个 `+ " "` 破坏。
