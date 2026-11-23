# 零依赖代码库索引

> 目标不是向量检索，而是给大仓库一张可复现的“相关文件地图”。
>
> 不引入 tree-sitter、embedding、向量库、数据库或 npm。

## 1. 命令

```bash
ui index
ui index --incremental
ui index --query "run_turn"
ui index --query "agent" --json
```

索引写入：

```text
.ui/index/manifest.json
```

## 2. 内容

每个文件记录：

```json
{
  "path": "src/agent.rs",
  "bytes": 12345,
  "lines": 400,
  "mtimeMs": 1790000000000,
  "sha256": "...",
  "symbols": ["fn run_turn", "struct Agent"]
}
```

符号提取是保守的正则/前缀匹配：

- Rust：`fn` / `struct` / `enum` / `trait` / `impl`；
- JS/TS：`function` / `class` / `const`；
- Python：`def` / `class`；
- Markdown：一级标题。

不解析语法树，不保证语言完整；它的作用是**排序候选文件**，不是替代 `grep`。

## 3. 排除与限制

- 排除 `.git/.ui/target/node_modules/dist/build`；
- 不跟随符号链接；
- 单文件超过 1 MiB 跳过；
- 二进制文件跳过；
- 每文件最多 40 个符号。

## 4. 注入 prompt

`prompt::build_snapshot` 会根据最近一条 user 输入查询索引，命中时追加：

```text
## 相关文件（轻量索引；不是全文）
- src/agent.rs (400 行) — fn run_turn, struct Agent
```

只注入路径/行数/符号，不注入文件正文。索引不存在时该段完全消失，旧工作区行为不变。

## 5. 为什么不用向量库

- 需要第三方 crate / 模型 / 服务；
- 不可审计；
- 大仓库更新成本高；
- 我们只需要“先看哪几个文件”，不需要语义相似度。

## 6. 边界

- 不做跨文件引用解析；
- 不做调用图；
- `--incremental` 按 size+mtime 复用未变文件；不是 watcher，仍需显式运行；
- 查询是字符串匹配，可能漏掉同义词。

## 7. 测试

- `index_finds_symbols_and_ignores_ui_internal`
- `missing_index_is_a_clear_error`
- `incremental_reuses_unchanged_and_updates_changed`
- CLI e2e：`index_build_and_query_end_to_end`
- prompt e2e：`index_is_injected_into_prompt_snapshot`
