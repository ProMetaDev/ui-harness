# B 组基准：3 个 CPU 任务 × 3 个 seed

目的不是刷分，是证明**工具链闭环**：跑实验 → 打包 → 校验 → 出表，每一步都不靠自述。

## 任务

| 任务 | 做什么 | 产物 |
|---|---|---|
| `lr` | 逻辑回归，400 样本 8 维，sklearn | `metrics.json{accuracy, n_train, n_test}` |
| `mlp` | 两层 MLP（numpy 手写反向传播，120 轮，隐层 16） | `metrics.json{accuracy, final_loss}` |
| `ablation` | MLP 扫 3 个学习率（0.01/0.1/0.5），取最佳 | `metrics.json{best_accuracy, best_lr}` |

全部 CPU、单次 < 2 秒、按 `--seed` 确定。

## 结果（脚本化模型，`tools/bench_ml.sh`）

```
lr        accuracy      3  0.8433  ±0.0881   0.72, 0.92, 0.89
mlp       accuracy      3  0.9600  ±0.0163   0.94, 0.96, 0.98
ablation  best_accuracy 3  0.9767  ±0.0125   0.98, 0.96, 0.99
```

每个 run 一个包，包里 1 个产物（`metrics.json`）、1 条命令、22 个日志事件。

## 真模型对照（`tools/bench_real.sh`）

同样的 `lr / seed=1`，让真模型自己干活，不做剧本：

```
模型  实验已完成：`python3 train.py --seed 1` 运行成功，标准输出为 `accuracy=0.7200`
准确率 0.72（72%），测试集 100 条样本
external │ tok 4269/200000 │ calls 3/60 │ 8s
结束原因 done · 3 步 · 3 次工具
```

打包后：

```
产物 1 个 · 命令 1 条 · 日志 22 事件
验证：产物 1 个校验通过 · 输入 2 个仍一致 · 日志链 完整
```

它的 0.72 与剧本跑出来的 seed 1 完全一致——**数字不是编的，产物里写着**。

## 两个真实发现

1. **`node` 不在非登录 shell 的 PATH 里**（nvm 只在 `.bashrc` 生效），适配器用 `sh -c` 启动会报 127。
   修法：`UI_MODEL_CMD` 里用绝对路径。
2. **一次失败的 run 被校验抓住了**：模型进程没起来，日志里留着 `turn/start` 没有对应 `turn/end`，
   `ui lab verify` 直接返回 3 并写明"日志里有未完成的调用"。这正是我们要的行为——
   **崩溃残留的实验不能被当成结果**。

## 已知短板

- **脚本产物的血缘是弱证据**：`metrics.json` 由 shell 命令写出，没有 `fs.write` 事件，
  只能靠 mtime 推断，清单里标 `provenance: "mtime"`。要变强，得让工具知道"命令会写哪些文件"。
- **报告里的数字没有自动对账**：模型说 0.72，产物里也是 0.72，但这是人工核对的。
  下一步可以做 `lab verify --claims`：把报告里的数字与产物逐个比对，对不上就判无效。
- `ablation` 的 `best_lr` 三个 seed 都是 0.5——任务太简单，区分度不够，后续换数据集再调。

## 真实多仓库评测：仍未完成，只有框架

上面的 3 个 CPU 任务和单仓库 `lr` 对照都**不是多仓库评测**。10 个真实仓库 × 12 个任务的
正式评测目前**一次都没跑**，没有任何可对外引用的真实结果。

已经落地的只有框架与合成 fixture 自测：

- `tools/bench_repos.py`：仓库校验、`ui index` 指标、`ui run` 记录、`ui verify` 校验、
  expect 判定、失败隔离，输出 `report.json` / `report.md`；
- `bench/repos.example.json`：10 个真实仓库候选，路径全是 `REPLACE_ME/...`；
- `bench/tasks.example.json`：12 个通用任务；
- `tools/make_bench_fixtures.sh` + `tools/verify_bench_repos.sh`：12 个合成 fixture 的
  dry-run/index 自测；fixture **不是真实仓库**，不产生真实数字。

没有 provider 时只做 index，报告 `mode=dry-run`、任务状态 `dry-run`、`success=0`；
这表示“没有执行模型”，不能当成失败率，也不能写进对外评测结论。完整口径、选仓标准、
带外试用记录字段见 [docs/56](56-real-repos.md)。
