---
id: mod-payment
title: 支付模块
source_sha: 
generated_at: 2026-11-17T20:00:00Z
generated_by_seq: 12
confirmed_by: human
touches: ["src/pay/**", "src/billing/*.rs"]
---

# 支付模块（示例卡）

这是 `docs/knowledge/` 的格式示例，也是「可审计的知识」的最小形态：
**人写的、带来源绑定的知识产物**，不是模型从仓库里自动总结出来的规范。

## 这张卡说的三件事

- **入口**：收款只有一条路径 `src/pay/charge.rs` 的 `pay::charge(order)`；
  退款也只有一条 `pay::refund`，两步确认（先锁单，再打款）。
- **约束**：金额一律用整数分（`i64`），浮点只允许出现在展示层。
- **禁忌**：不要在 `pay/**` 里直接调 `billing::invoice`——会绕过分账校验
  （见日志 seq 8 的那次返工）；`billing/*.rs` 改动后必须重跑 `cargo test -p billing`。

## 为什么 `source_sha` 是空的

`touches` 指的是本仓库里**不存在**的 `src/pay/`、`src/billing/`（这张卡只是格式示例），
所以它没有可绑定的源码，`source_sha` 留空。`ui knowledge check` 会把它标成
`UNBOUND`（没有绑定），而不是 `STALE`（绑定过后又对不上）。

真实卡片应该这样写：

```sh
ui knowledge check          # 打印每张卡的一致性；有 STALE 时退出码 2
```

把 `ui knowledge check` 算出来的哈希（完整 64 位，或前 40 位）填进 `source_sha`，
就得到了「这张卡描述的是哪一份源码」的证据。之后任何一个 `touches` 命中的文件被改动，
`check` 都会立刻报 `STALE`——过期知识不会静默地继续被当成规范使用。

## 事件

- `knowledge/emit`：卡片发布时写一条（`card` / `source_sha` / `generated_by_seq` /
  `confirmed_by` / `touches`）；
- `knowledge/use`：`ui run` 的任务前预取命中，或手工 `ui knowledge use` 时写一条
  （`card` / `source_sha` / `reason` / `budget_tokens` / `score`）。

两条事件都走现有哈希链，所以「这次回答用了哪张卡、凭什么命中、当时对应哪份源码、
谁确认过」可以顺着 `seq` 往回读，并且可以用 `ui verify` 证明没有被改过。
