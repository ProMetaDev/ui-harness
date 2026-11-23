# Checkpoint / Diff / Review / Restore：零依赖变更审阅地基

> 这是“Cursor 式变更审阅”的第一层，不是完整 IDE。
>
> 目标：让 Agent 改了什么**可看见、可对比、可回滚**，且不引入 git、npm、第三方 crate。

## 1. 命令

```bash
ui run --task "..." --checkpoint   # 开跑前自动存基线
ui checkpoint --label before-edit
ui checkpoints
ui diff
ui diff --checkpoint cp-... --json
ui review --checkpoint cp-...
ui review --checkpoint cp-... --accept path#1,path#2 --yes
ui review --checkpoint cp-... --accept all --yes
ui revert --checkpoint cp-... --path src/main.rs --yes
ui restore --checkpoint cp-... --yes
ui restore --checkpoint cp-... --prune --yes
```

### checkpoint

- **自动**：每个 turn 第一次写操作（fs_write/patch/shell/plugin/mcp 等）前
  自动建 `auto-before-turn-<n>`；成功写 `checkpoint/auto`，失败写
  `checkpoint/error`（不会假装基线存在）；
- `UI_AUTO_CHECKPOINT=0` 或 `--no-auto-checkpoint` 关闭；
- `ui run --checkpoint` 额外在开跑前存一次 `before-run`；
- 也可手动 `ui checkpoint` 指定 label；
- 扫描 workspace；
- 每个文件内容做 sha256；
- 内容写入 `.ui/checkpoints/blobs/<sha256>`；
- manifest 写入 `.ui/checkpoints/<id>/manifest.json`；
- 相同内容只存一份。

排除：

- `.git`
- `.ui`
- `target`
- `node_modules`

不跟随符号链接；符号链接不会进入 checkpoint。

### diff

- 默认对比最新 checkpoint；
- 输出 `added / deleted / modified`；
- 文本文件生成 unified diff；
- 二进制文件只报告 differ；
- 文件过大/行数过多时降级为字节数提示，不生成逐行 hunk。

### review

- 列出 hunk，hunk id 形如 `src/main.rs#1`；
- `--accept path#n,...`：只保留选中的 hunk，其余 hunk 回退到 checkpoint；
- `--accept all`：接受全部；
- 二进制/过大文件没有逐行 hunk，可用文件名作为文件级接受标记；
- 写盘前自动创建 `pre-review-<id>`；
- 先计算全部结果再写盘，hunk 上下文不匹配时不会留下半套修改；
- 成功后写一条 `review/apply` journal 事件（actor=cli，含 checkpoint/accepted/written/removed/backup）；
- 审计写失败时明确报“文件已写回、需要人工补账”，不静默成功；
- 必须显式 `--yes`。

### revert

- 只处理一个文件：manifest 里有就写回旧内容，没有（checkpoint 后新增）就删除；
- 拒绝 `.ui/.git/target/node_modules`、绝对路径和 `..`；
- 写盘前自动创建 `pre-revert-<id>`；
- 成功后写 `review/revert` journal 事件；
- 必须显式 `--yes`。

### restore

- 先自动创建一个 `pre-restore-<id>` checkpoint；
- 只写回 manifest 中存在的路径；
- 默认不删除 checkpoint 之后新增的文件；`--prune` 才清理未跟踪文件；
- 内容完全相同的 delete+add 会识别为 `renamed`（改名又改内容仍按 delete+add）；
- 拒绝写符号链接或越出 workspace 的路径；
- 必须显式 `--yes`。

## 2. Web 端

聊天页顶部「变更」按钮：

- 选择 checkpoint；
- 查看文件级 diff；
- 每个 hunk 一个复选框；
- 每个文件有「回退此文件」按钮，走 `POST /api/revert`；
- 「应用选中」走 `POST /api/review`，未选中的 hunk 回退；
- 任务运行/排队时拒绝 review（409），避免和 agent 同时改文件；
- diff 内容全部用 `textContent` 渲染，不把代码当 HTML。

API：

```text
GET  /api/checkpoints
GET  /api/diff?checkpoint=<id>
POST /api/review
     {"checkpoint":"cp-...","accepted":["src/main.rs#1"],"confirm":true}
POST /api/revert
     {"checkpoint":"cp-...","path":"src/main.rs","confirm":true}
```

`POST /api/review` 需要同源请求 + token + `confirm:true`；写盘前自动建
`pre-review-<id>`，成功后写 `review/apply` journal 事件（actor=web），并在响应
里返回 `journalSeq`。

## 3. 数据布局

```text
.ui/checkpoints/
├── blobs/
│   └── <sha256>
└── cp-<timestamp>-<hash>/
    └── manifest.json
```

manifest：

```json
{
  "label": "before-edit",
  "createdAt": 1790000000000,
  "workspace": "/path/to/ws",
  "entries": [
    {"path": "src/main.rs", "sha256": "...", "bytes": 1234}
  ]
}
```

## 4. 为什么不用 git

- git 是外部依赖；
- 工作区可能不是 git repo；
- 我们只需要“文件内容快照”，不需要分支/历史/远程；
- 内容寻址 blob 更简单、可审计、可删除。

## 5. 与 Cursor 的差距

已吸收：

- 变更可见；
- 文件级 diff；
- hunk 级 accept/reject；
- 可回滚；
- JSON 输出供 UI 消费。

还没做：

- 逐文件 revert；
- 代码库索引；
- 内联 UI；
- 文件 mode/重命名检测；
- prune 未跟踪文件。

## 6. 安全边界

- checkpoint 只读 workspace 文件，不跟随 symlink；
- restore 校验父目录 realpath；
- restore 不写 symlink 目标；
- manifest 路径拒绝绝对路径和 `..`；
- 所有写入走临时文件 + rename；
- `.ui` 自身不进入 checkpoint。

## 7. 测试

- `checkpoint_diff_restore_round_trip`
- `review_keeps_only_accepted_hunks`
- `review_accepts_binary_by_file_path`
- `review_rejects_unknown_hunk_without_writing`
- `restore_refuses_to_write_through_a_symlink`
- `symlinks_are_not_followed`
- CLI e2e：`checkpoint_diff_restore_end_to_end`
- CLI e2e：`review_accepts_only_one_hunk_end_to_end`
- CLI e2e：`revert_single_file_end_to_end`
- CLI e2e：`run_can_checkpoint_before_tool_writes`
- CLI e2e：`run_auto_checkpoints_before_first_write`
- `automatic_limit_fails_before_writing_a_manifest`
- `restore_prune_removes_untracked_files_only_when_asked`
- `exact_content_rename_is_detected_and_reviewed`
- HTTP e2e：`checkpoint_diff_review_api_round_trip`
- HTTP e2e：`web_revert_single_file_is_audited`

## 8. 下一步

1. hunk accept/reject 的 Web 视图；
2. file-level revert；
3. rename detection；
4. 和 Web 的 diff 视图打通；
5. codebase index（仍 std-only）。
