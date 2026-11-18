# tools/ 索引

`tools/` 是仓库的脚本层：没有构建系统魔法，都是可以直接读的 bash / Python / Node 脚本。
先记住四个入口，其余按类别查表即可：

```bash
bash tools/test.sh        # 构建副本里跑 cargo test
bash tools/debug.sh       # 门禁（构建 + 端到端 + 攻击场景 + 跨平台；项数以输出为准）
bash tools/ci.sh          # 本地 CI 等价物（GitHub Actions 直接调用它）
bash tools/demo-audit.sh  # 30 秒演示：跑一轮 → 回放 → 校验 → 篡改 → 发现
```

## 分类

| 类别 | 按文件名怎么认 | 代表文件 | 说明 |
|---|---|---|---|
| **CI / 构建 / 发布** | `ci.sh`、`dev.sh`、`sync.sh`、`preflight.sh`、`test.sh`、`ct.sh`、`bootstrap.sh`、`build_windows.sh`、`install_rustup_mirror.sh`、`release*.sh`、`tag.sh`、`changelog.sh`、`backup.sh`、`tidy.sh`、`env.sh`、`where.sh`、`stats.sh` | `ci.sh` → `debug.sh` → `release.sh` | 构建走 `sync.sh` 到 ext4 副本；`env.sh` 只被 source，统一路径与 cargo。 |
| `ci_mingw.sh` | CI 专用：下载 posix 变体的 .deb 解包到 `~/mingw64`（可缓存），不 install |
| **门禁 verify** | `verify_*`（`.sh` / `.py` / `.ps1`）、`check_*`、`kit.sh`、`count_tests.sh`、`scan_secrets.sh`、`serve_smoke.sh`、`tui_smoke.sh` | `verify_lineage.sh`、`verify_mcp.sh`、`verify_sandbox.sh`、`kit.sh` | 一项检查一个脚本，非 0 退出即失败；`debug.sh` 是逐项显式调用它们的总入口。 |
| **基准 bench** | `bench_*`（`.sh` / `.py`）、`soak.sh`、`stability_*.sh`、`eval.sh`、`cost_report.mjs`、`ab_report.sh` | `bench_ml.sh`、`bench_self.sh`、`soak.sh` | 输出数据/报告，不当作通过/失败门禁；结论要能被 `verify_claims.sh` 对账。 |
| **适配器 adapters** | `*.mjs`、`probe_adapter*.sh`、`stream_e2e.sh`、`smoke_external.sh` | `deepseek.mjs`、`chat_compat.mjs`、`fake-chat.mjs`、`fake_deepseek.mjs` | 把外部模型/端点包成 `UI_MODEL_CMD` 可用的进程；`fake-*` 用于离线复现。 |
| **真机 / 演示** | `*real*.sh`、`demo*.sh`、`tui_*`、`lab_real.sh`、`team_*.sh`、`acceptance*.sh`、`export_demo.sh`、`preview.sh`、`proto.sh`、`consult.sh`、`replay_screen.mjs`、`gen_image.mjs` | `demo-audit.sh`、`lab_real.sh`、`team_real.sh` | 需要真模型（`secrets/astra.json`）、真终端或截图环境，**不进 CI**。 |
| **一次性开发脚本** | `_` 前缀 | 本次清理删除了 8 个 | 开发期临时驱动。规则：不提交、不被 `debug.sh` / `ci.sh` / 文档引用。 |

## 命名约定（新增脚本时请遵守）

- `verify_<主题>.sh|py`：一项可自动判定的检查，退出码就是结论。
- `bench_<主题>.sh|py`：基准/浸泡，只产出数据，不判定通过。
- `*_real.sh`：需要真实模型或真实终端，默认不跑。
- `_<名字>`：一次性脚本，留在本地，不要提交。

`debug.sh` 不会自动扫描目录，它逐项调用固定路径。**新增门禁必须手动加进 `debug.sh`**，
否则不会被执行；反过来，删除一个脚本前要先确认它没有被 `debug.sh`、`ci.sh` 或文档引用。

## 文件数

本次清理删除了 8 个零引用的一次性脚本（见仓库提交记录）。当前文件数以
`git ls-files tools | wc -l` 为准；上表只做归类，不替代清单。
