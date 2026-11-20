# 33 · Word（DOCX）安全编辑：方案与验证

> Skill：`demo/skills/docx-safe-edit/SKILL.md`
> 脚本：`demo/skills/docx-safe-edit/scripts/docx_safe.py`
> 通用约束：`demo/skills/doc-safe-edit/SKILL.md`

## 1. 为什么 Word 不能“整篇重写”

DOCX 不是一段文本，而是 ZIP + OOXML：

```
word/document.xml          正文
word/styles.xml            样式
word/numbering.xml         编号
word/header*.xml           页眉
word/footer*.xml           页脚
word/media/*               图片
word/_rels/document.xml.rels
```

一个段落的文字可能被拆成多个 `<w:r>` / `<w:t>`，中间夹着书签、域代码、批注、格式切换。
如果用 `Document()` 新建文档再拼正文，或者 Pandoc 往返，内容看似还在，但：

- 样式、编号、页眉页脚、节属性会丢；
- 图片关系、超链接、书签会断；
- 表格/段落结构会被重排；
- 原文的“可打开性”可能没问题，但“还是不是原文档”已经不是了。

## 2. 方案：copy-on-write + 只改 document.xml

### 步骤

1. `inspect`：统计 zip 完整性、段落/表格/图片/节/页眉页脚、关系目标；
2. `copy`：同目录复制为 `report.edit-YYYYMMDD-HHMMSS.docx`，比对 sha256；
3. `replace --dry-run`：在副本上找目标短语，只报命中数；
4. `replace`：只改 `word/document.xml`，其余 zip part 逐字节复制；
5. `validate`：原件哈希、zip、结构计数、关系、文本变化；
6. 默认只交副本；用户确认后才备份原件并替换。

### 为什么不用 python-docx 重建

`python-docx` 适合常规段落/表格编辑，但：

- `paragraph.text = ...` 会重建 runs；
- 对不支持的 OOXML 元素，不保证完整保留；
- 高层 API 方便，但“看不见的保真”不是它的契约。

所以脚本选择 **lxml 直接改 XML**，只碰 `<w:t>` 文本节点；样式/编号/关系完全不动。

## 3. 跨 run 替换算法

对每个 `<w:p>`：

1. 按文档顺序收集所有 `<w:t>`，拼成段落文本；
2. 在拼接文本中找 `find`；
3. 把命中的字符区间映射回各 `<w:t>`；
4. 起始节点写入 `前缀 + 替换文本`，中间节点清空，结束节点保留后缀；
5. 设置 `xml:space="preserve"`（首尾空格）；
6. 从右向左应用多个命中，避免偏移失效。

这保证替换后**段落/表格/图片/关系结构不变**，代价是：

> 被替换短语内部的格式（粗体/颜色/超链接/域）会折叠到起始 run。

这不是 bug，是取舍。不能接受时，拆成更小的 run 级替换，或让用户确认。

### 3.1 页眉 / 页脚

`replace` 默认只改 `word/document.xml`；加 `--part` 可改指定 XML：

```bash
unzip -l report.docx | grep -E 'word/(header|footer)'
python3 docx_safe.py replace report.edit-....docx \
  --part word/header1.xml --find "旧标题" --replace "新标题"
```

只重写该 part，其他 part 逐字节复制。正文与页眉互不串改。

### 3.2 日期 / 金额格式化

```json
{"key":"活动时间","value":"2026-12-05 14:00|2026-12-05 16:00","format":"zh-date-range"}
{"key":"参与人数","value":"1234","format":"thousands"}
```

支持：

- `zh-date`：`2026-12-05` → `2026年12月5日`
- `zh-datetime`：保留时间
- `zh-date-range`：两个 ISO 日期用 `|` 或 `~` 分隔；同一天会自动压缩成 `14:00—16:00`
- `thousands` / `cny`：数字格式

解析失败就拒绝，不猜。

## 4. 表格：fill 而不是 overwrite

表格不能按“整格文本赋值”处理，否则 `w:tcPr` 的边框/底纹/边距/宽度、`w:trPr` 的行高、`w:tblPr` 的表格样式都可能被重写。

`table-fill` 的规则：

```bash
python3 docx_safe.py table-fill report.edit-....docx \
  --table 1 --row 2 --col 2 --value "8080"
```

1. `--table` / `--row` / `--col` 都是 1 起；`--col` 是逻辑列，跨列按 `gridSpan` 算；
2. 目标格为空或 `{{env}}` / `____` / `待填` 才写；
3. 已有内容默认拒绝，必须显式 `--overwrite`；
4. 空格的格式模板：同行标签格 → 同列表头；
5. 只改目标单元格的段落/run；`w:tcPr` / `w:trPr` / `w:tblPr` 不动；
6. 默认拒绝多段落已有内容；纵向合并延续格拒绝；
7. 单值里的 `\n` 转 `<w:br/>`；`paragraphs` 数组才是真段落。

实测：

- 表头 `字段 / 值` 粗体保持不变；
- 空值格填入 `8080`，继承“端口”标签格的普通字重；
- 占位符 `{{env}}` 填入 `prod`；
- `已填写` 格拒绝覆盖，退出 1；
- `validate` 的 `table_rows/table_cells/table_grid_spans` 全部不变。

### 4.2 语义定位

`table-fill-map` 支持按标签文本定位：

```json
{"key": "活动时间", "value": "2026年12月5日 14:00—16:00"}
```

- 在表格中找可见文本等于 `key` 的单元格；
- 默认目标 = 该标签格右边一个逻辑列；
- `活动时间` 在 C1 → C2；`报名时间` 在 C3 → C4；
- 同一个 key 出现多次直接拒绝，不猜；
- 显式 `col` 可以覆盖目标列；`row/col` 坐标仍可用。

这样不再需要把“活动时间在第 6 行第 2 列”写死。

批量填写：

```bash
python3 docx_safe.py table-fill-map config.edit.docx --map fill-plan.json
```

一份 JSON 里列出所有 `row/col/value`，一次复制、一次写入、一次校验；任意一格非空/非占位符，整批拒绝，不写半成品。

### 4.1 样式模板与多段

空单元格没有 run，默认会继承“同行标签格”。但长文本格（如“活动内容”）通常应该是正文样式，不是标签样式。

用 `styleFrom` 指向一份**同模板的已填写样例**，只复制格式，不复制正文：

```json
{
  "table": 1,
  "styleFrom": "submitted.docx",
  "fills": [{
    "row": 9,
    "col": 2,
    "templateRow": 9,
    "templateCol": 2,
    "paragraphs": ["第一段", "第二段", "第三段"]
  }]
}
```

`paragraphs` 会生成多个真实 `<w:p>`，每段从模板格复制 `w:pPr`；run 复制 `w:rPr`。  
如果目标模板缺少参考文档的样式 ID，Word 会回退，必须人工确认。

实测同一份申报表：

- 默认 fill：1 段、黑体、默认对齐；
- `styleFrom` 后：4 段、宋体、两端对齐，与真实提交案例一致；
- 表格行/列/合并/宽度/高度不变；`validate` 只报告 `paragraphs 42→45` 这一处预期增长。

### 4.3 硬保护

`replace` / `table-fill` / `table-fill-map` 默认拒绝目标文件名不含 `.edit` 的文件：

```
ERROR: 拒绝直接修改原件：目标文件名没有 .edit 标记；先 copy，或显式加 --force
```

`--force` 只用于恢复/特殊场景，不是日常捷径。这样 agent 即使误把原件路径传进来，也会在写盘前停住。

## 5. 验证脚本

```bash
python3 docx_safe.py inspect report.docx
python3 docx_safe.py copy report.docx
python3 docx_safe.py replace report.edit-....docx \
  --find "Hello world" --replace "Hi world"
python3 docx_safe.py validate report.docx report.edit-....docx
```

`validate` 输出：

```json
{
  "ok": true,
  "original_untouched": true,
  "zip_ok": true,
  "lost_structure": [],
  "changed": {},
  "text_chars": [52, 49]
}
```

## 6. 实测

用 python-docx 生成一个含标题、跨 run 粗体、表格、列表的样例：

- 原文 `Hello ` + 粗体 `world`，拆成两个 run；
- 替换 `Hello world` → `Hi world`；
- 结果：正文变成 `Hi world`，表格仍 1 个，段落数不变，原件 sha256 不变；
- 格式代价：`world` 的粗体折叠到起始 run（脚本不假装没发生）。

## 7. 边界

脚本明确不做：

- 页眉/页脚/脚注/批注里的替换；
- 跨段落替换；
- 保留被替换短语内部的 run 级格式；
- 视觉渲染校验（当前环境没有 Word/LibreOffice）；
- 删除表格/图片/节。

这些不是“以后再说”的模糊项，是 skill 的停止条件。

## 8. Agent 端到端实测（scripted model）

用一个真实的 `ui run`，但模型用 `--mock` 脚本，验证的是**协议与工具链**，不是模型理解能力：

```bash
ui run --task test --mock scenario.json --yes
```

场景：

1. `skill(name=docx-safe-edit)` 加载正文；
2. `shell` 跑 `inspect report.docx`；
3. `shell` 跑 `copy report.docx --out report.edit.docx`；
4. `shell` 跑 `replace report.edit.docx --find "Hello world" --replace "Hi world"`；
5. `shell` 跑 `validate report.docx report.edit.docx`。

结果：

- journal 中有 **1 条 `skill/load`**、5 条 `tool/end`、2 条 `effect/observed`；
- `effect/observed` 没有临时文件（temp 放进 `.ui/tmp`，被 observe 跳过）；
- `validate`：`ok=true`、`original_untouched=true`、`lost_structure=[]`、`text_chars 51→48`；
- 原件 sha256 未变，表格仍 1 个；
- 已知代价：跨 run 的 `world` 粗体折叠到起始 run。

## 9. 下一步

1. 加 `--run-wise` 模式：按 run 分段替换，保留被替换文本内部格式；
2. 用 LibreOffice 转 PDF/PNG 做视觉抽查；
3. 表格按表头名 + 行 key 定位（当前是标签右邻格）；
4. 把“原件未变/结构守恒”写进 `ui verify` 的审计事件。

## 10. 真实申报表对比（2026-11-21）

拿同一张《南昌大学第二课堂活动申报表》做了对比：

- 模板结构：12 行、32 个物理单元格、8 个 gridSpan、0 个 vMerge；两边一致；
- 列宽、行高、表格边框/底纹/合并关系：未改；
- 原件 sha256 `3655de7a…` 未变；
- 差异主要在**内容与长文本排版**。

| 项 | 真实提交 | 默认 fill | `styleFrom` 后 |
|---|---|---|---|
| 活动时间 | `2026年10月16日 19:00—20:00` | `2026-12-05 14:00` | `2026年12月5日 14:00—16:00` |
| 报名时间 | `2026年10月14日 —2026年10月16日` | `2026-11-28 09:00` | `2026年11月28日 09:00—2026年12月3日 17:00` |
| 参与对象 | 具体班级全体同学 | 软件学院本科生 | 软件学院本科生 |
| 参与人数 | 16 | 80 | 80 |
| 活动内容 | 4 段、约 300 字、宋体、两端对齐 | 1 段、约 40 字、黑体、默认对齐 | 4 段、宋体、两端对齐 |

结论：

- **结构层面**已经对齐：表格没有重建，行/列/合并/宽度/高度都保留；
- 默认 fill 会取“同行标签格”的格式，长文本格会变成黑体/单段；
- **`styleFrom` 已经补上**：用真实提交件作参考后，活动内容变成 4 段、宋体、两端对齐，与样例一致；
- 日期格式仍是内容问题，不是 fill 引擎问题；需要由填写清单提供目标格式；
- 当前环境没有 Word/LibreOffice，未做视觉渲染。

`styleFrom` 的批量写法：

```json
{
  "table": 1,
  "styleFrom": "submitted.docx",
  "fills": [{
    "row": 9,
    "col": 2,
    "templateRow": 9,
    "templateCol": 2,
    "paragraphs": ["第一段", "第二段"]
  }]
}
```

## 11. 参考

- [Editing existing DOCX via OOXML](https://github.com/openkursar/hello-halo/blob/a1b6d395e9c3609d33cefa0fd2fb99d280bfb4e0/resources/builtin-skills/docx/references/editing.md)
- [docx-official/edit.md](https://github.com/xiaomimimo/mimo-code/blob/5a538cb4f97b9c4c8eb09a845ec06c1e5f1f3e8f/packages/cli/src/skill/builtin/.bundle/docx-official/edit.md)
- [python-docx](https://python-docx.readthedocs.io/)
