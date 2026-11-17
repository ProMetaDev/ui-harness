# 变更记录

由 `bash tools/changelog.sh` 从 git 历史生成，不要手改。
每条按类型分组：新功能 / 修复 / 工程 / 文档。

## v0.1.0（首个版本） · 2026-11-16

**工程**

- release packaging with self-verify, secret scanner, user-facing README (3 parallel agents)
- watch git refs so --version reflects HEAD
- recompile when git HEAD moves so --version stays honest
- portable paths, --version with commit, strip (15M->1.9M), preflight, dynamic ports

**文档**

- engineering status (P0/P1 done, strip done)
- engineering analysis (8 issues, costed plan)
- status snapshot (per-area completion)
- gap analysis (capability / evidence / engineering)

**其他**

- 7-attack verification pack + independent python journal verifier + docs/22
- trust boundary in doctor+security; team apply opt-in; consult analysis
- generic OpenAI-compatible adapter + review brief + runner
- CLI + web switching (journal per session, history rebuilt); test.sh now propagates cargo exit code
- Windows second pass: real model + serve verified; fix cmd quoting and console UTF-8
- Windows 实机: ui.exe 跑通；修 BOM/CRLF patch/grep 退化/路径显示；文档 18-windows
- 验证: 契约自检 T1-T7 实跑 21/21（含真模型 T4）
- G: 插件系统（进程 + 静态能力声明，plugin_call，拒绝非法声明）
- C: provider 注册表（.ui/providers.json + --provider + ui providers）
- F: 工具面 7 -> 12（grep/patch/todo/tree/fs_mv）
- A3/D/H: --append-system-prompt、子进程 env 清理、Apache-2.0 许可；ui prompt 调试命令
- A4: 工具白名单（--no-tools/--tools）+ 用别人的尺子自评（57/81）
- 对标: 开源 harness 全景（9 个被同一基准量过）+ 自评
- --accept 验收闸（失败返回 3）+ DSH 能力面对标
- plan 工具提交计划（协议内）+ 自动 apply + apply 后汇总；真模型实测
- 协作模式: ui team（拆解 -> workflow 并行隔离 -> 汇总）+ 门禁 + 文档
- 鉴权: 默认要求 token，?t= 种 Cookie，--token off 可关
- 宽度 >=100 时的「当前步」右栏，并修复终端尺寸探测
- 工具: 清理一次性脚本
- 工具: 仓库初始化、备份（双盘镜像）、文档 12-repo
- 首个可用原型

## v0.1.1 · 2026-11-16

**工程**

- untracked scratch files no longer mark a build dirty
- dump the failing step log into the diagnostic step (and fix the log path)
- keep step log at a stable path so the failure artifact has content
- on failure print the last 40 lines of the failing step
- call tools/ci.sh instead of duplicating steps (version check ran before build)
- drop remaining scratch scripts
- remove internal scratch scripts from the repo
- clean temp scripts before first push
- build before version check; gate count is dynamic
- local ci.sh + GitHub Actions workflow; sync skips when source==target

**文档**

- note that CI is verified in a clean checkout

**发布**

- rebuild 0.1.1 from the tagged commit; refresh status numbers
- v0.1.1 (first green CI: journal multi-writer race fixed)

**其他**

- read length under the same lock as the read (CI caught a real multi-writer race)
- viewer check uses python3; drop the deleted _node.sh dependency
- mcp test uses an absolute fixture path; drop workspace warnings

## v0.1.2 · 2026-11-16

**新功能**

- observed effects for shell/plugin/mcp (effect/observed; UI_OBSERVE=0 to disable)

**工程**

- remove scratch scripts
- rebuild after README update
- cross-compile ui.exe on every push (minGW on ubuntu runner)

**文档**

- pre-public pass (SECURITY.md, sanitize personal path, version bump)
- status snapshot at v0.1.1 (CI green, real-model lab verified)

**其他**

- artifacts were nested twice and counted as inputs (found by a real model run)

## v0.1.3 · 2026-11-16

**新功能**

- OS sandbox on Linux via Landlock (fail-closed; zero deps, no root)

**文档**

- hands-on audit walkthrough (demo-audit.sh + quickstart)
- use a real conversation as hero; fix screenshot labels
- rewrite README around the problem it solves; add screenshots
- generated CHANGELOG (tools/changelog.sh)

## v0.1.4 · 2026-11-17

**文档**

- windows sandbox plan (3 tiers, risks, acceptance)

**其他**

- Job Object limits + guaranteed tree reclamation (no orphans on normal exit)

## v0.1.5 · 2026-11-17

**修复**

- cap journal memory buffer (soak found 5x RSS growth); add --max-steps; soak tool + report

## v0.1.6 · 2026-11-17

**修复**

- journal write failures are fail-closed with actionable errors (41 gate items)

## v0.1.7 · 2026-11-17

**其他**

- streaming verify/replay (memory O(1)); bench harness; perf report

## v0.1.8 · 2026-11-17

**工程**

- annotate the FAIL lines and the gate summary
- emit failure tail as annotations so it can be read via API
- put failure diagnostics into the run summary

**其他**

- a missing journal is an empty one, not an error (CI caught it)
- 飞行记录仪界面落地（日志/当前步右栏/固定审批/动效）
- unify cargo, build windows in the copy, tolerate env-less landlock signal deaths
- journal open scans only the tail; catch_up reads from offset (O(1) memory on 300k-event logs)
- 克制的动效系统（入场/数据生长/审批条/盖章）+ 动效录屏
- gpt-image-2 方向板 + 纸面实验室编号/验证章 + UU 设定

## v0.1.9 · 2026-11-17

**工程**

- drop scratch scripts

**其他**

- observation via inotify (85ms -> 5ms per call on 20k files), scan fallback

## v0.1.10 · 2026-11-17

**其他**

- verify reads only seq/prev/kind/callId (549ms vs 664ms on 300k events)
- 视觉精修（masthead/字阶/日志式工具行/1280 仪表条）

## v0.1.11 · 2026-11-17

**工程**

- lockfile 0.1.10

**其他**

- streaming export (112MB -> 3.5MB on 70k events); refuse to export a tampered journal
- 流式与右栏动效按帧合并，消除整块重绘的卡顿感

## v0.1.12 · 2026-11-17

**其他**

- copy and hash in one pass (incremental sha256); fix mixed build roots
- 树结构视觉层（右栏 spine/节点码、时间轴节点）
- tag by explicit SHA (parallel work makes HEAD unsafe)
- 信息架构收口（4 个真入口、会话单职责、低频操作进更多菜单）

## v0.1.13 · 2026-11-17

**其他**

- xxHash64 manifest + content-based changed detection (-20%); fixes the >2MB blind spot
- GPT 数据树背景板 + 缓慢漂移/光晕，背景随二进制走

