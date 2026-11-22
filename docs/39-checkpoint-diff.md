# Checkpoint / Diff / Restore：零依赖变更审阅地基

> 这是“Cursor 式变更审阅”的第一层，不是完整 IDE。
>
> 目标：让 Agent 改了什么**可看见、可对比、可回滚**，且不引入 git、npm、第三方 crate。

## 1. 命令

```bash
ui checkpoint --label before-edit
ui checkpoints
ui diff
ui diff --checkpoint cp-... --json
ui review --checkpoint cp-...
ui review --checkpoint cp-... --accept path#1,path#2 --yes
ui review --checkpoint cp-... --accept all --yes
ui restore --checkpoint cp-... --yes
```

### checkpoint

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
- 必须显式 `--yes`。

### restore

- 先自动创建一个 `pre-restore-<id>` checkpoint；
- 只写回 manifest 中存在的路径；
- **不删除** checkpoint 之后新增的文件（没有 `--prune`）；
- 拒绝写符号链接或越出 workspace 的路径；
- 必须显式 `--yes`。

## 2. 数据布局

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

## 3. 为什么不用 git

- git 是外部依赖；
- 工作区可能不是 git repo；
- 我们只需要“文件内容快照”，不需要分支/历史/远程；
- 内容寻址 blob 更简单、可审计、可删除。

## 4. 与 Cursor 的差距

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

## 5. 安全边界

- checkpoint 只读 workspace 文件，不跟随 symlink；
- restore 校验父目录 realpath；
- restore 不写 symlink 目标；
- manifest 路径拒绝绝对路径和 `..`；
- 所有写入走临时文件 + rename；
- `.ui` 自身不进入 checkpoint。

## 6. 测试

- `checkpoint_diff_restore_round_trip`
- `review_keeps_only_accepted_hunks`
- `review_accepts_binary_by_file_path`
- `review_rejects_unknown_hunk_without_writing`
- `restore_refuses_to_write_through_a_symlink`
- `symlinks_are_not_followed`
- CLI e2e：`checkpoint_diff_restore_end_to_end`
- CLI e2e：`review_accepts_only_one_hunk_end_to_end`

## 7. 下一步

1. hunk accept/reject 的 Web 视图；
2. file-level revert；
3. rename detection；
4. 和 Web 的 diff 视图打通；
5. codebase index（仍 std-only）。
