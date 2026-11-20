---
name: docx-safe-edit
description: Word 文档先复制再改：只动 word/document.xml 的文本节点，保留样式/表格/图片/关系，改完做结构校验
---

# Word（DOCX）安全编辑

适用于修改已有 `.docx`，不适用于从零新建文档。

## 0. 先记住 DOCX 是什么

`.docx` 是一个 ZIP：

```
[Content_Types].xml
_rels/.rels
word/document.xml          ← 正文
word/styles.xml            ← 样式
word/numbering.xml         ← 编号
word/header1.xml, footer1.xml
word/media/*               ← 图片
word/_rels/document.xml.rels
```

正文里一个段落是 `<w:p>`，一个 run 是 `<w:r>`，文字在 `<w:t>`。
**一个词可能被拆在多个 `<w:r>/<w:t>` 里**，所以不能用“整段替换”的方式改。

## 1. 复制工作副本（必须）

```bash
python3 <skill>/scripts/docx_safe.py inspect report.docx
python3 <skill>/scripts/docx_safe.py copy report.docx
# → report.edit-YYYYMMDD-HHMMSS.docx
```

- 副本放在原文件同目录，保留相对图片/模板关系；
- 原件只读；交付前必须校验原件 sha256 不变；
- 不要用 `fs_write` 重建 `.docx`，不要用 Pandoc 往返。

## 2. 先查，不要直接改

```bash
python3 <skill>/scripts/docx_safe.py inspect report.docx
```

确认：

- 段落 / 表格 / 图片 / 节 / 页眉页脚数量；
- 是否有 styles/numbering；
- 关系目标是否都存在（脚本会列 `missing_rel_targets`）；
- 目标文字是否在正文，而不是页眉/脚注/批注。

脚本只处理 `word/document.xml`。如果目标在页眉、页脚、脚注、批注、文本框外链里，**停**，不要假装改到了。

## 3. 最小替换

先 dry-run：

```bash
python3 <skill>/scripts/docx_safe.py replace report.edit-....docx \
  --find "旧文本" --replace "新文本" --dry-run
```

再执行：

```bash
python3 <skill>/scripts/docx_safe.py replace report.edit-....docx \
  --find "旧文本" --replace "新文本"
```

脚本行为：

- 只在副本上操作，写入临时文件后原子替换副本；
- 只改 `word/document.xml`；styles、numbering、media、rels、页眉页脚全部逐字节复制；
- 支持跨 `<w:r>` 的短语：替换文本落在起始 run，后续 run 清空；
- 不处理跨段落短语；找不到就退出，不写文件。

### 格式损失必须说清楚

跨 run 替换会把整段替换文本归到**起始 run 的格式**。
如果被替换的短语内部有粗体/颜色/超链接/域代码，替换后可能丢失内部格式。

遇到这种情况：

- 不要假装“格式没变”；
- 先拆成多个更小的 `--find`，逐个 run 改；
- 或者停下来，让用户确认是否接受格式折叠。

## 4. 校验（强制）

```bash
python3 <skill>/scripts/docx_safe.py validate report.docx report.edit-....docx
```

必须满足：

- 原件 sha256 与编辑前一致；
- 副本 `unzip -t` 通过（脚本内部已测）；
- 段落 / 表格 / 图片 / 节 / 页眉页脚数量不减少；
- styles / numbering / media / relationships 都还在；
- 文本长度变化只来自预期替换。

任何一项失败：保留原件，停止交付。

没有 LibreOffice 时，脚本只能做结构校验，不能做视觉校验。必须明确告诉用户：

> 结构校验通过，但未做版面渲染抽查；请在 Word 里打开副本确认。

## 5. 交付

默认只交副本：

```
原件：report.docx（sha256 未变）
副本：report.edit-20261120-234500.docx
改动：正文 "旧文本" → "新文本"（1 处）
结构：段落 9→9，表格 1→1，图片 0→0，节 1→1
校验：通过（未做 Word 版面渲染）
```

只有用户明确说“替换原件”才：

1. 把原件移到 `report.bak-<timestamp>.docx`；
2. 副本原子移动到 `report.docx`；
3. 报告备份路径与新 sha256。

不要默认覆盖，不要删除副本/备份。

## 6. 停止条件

- 原件哈希已变；
- 目标不在 `word/document.xml`；
- 需要跨段落替换；
- 替换会删除表格/图片/节；
- 需要保留被替换短语内部格式，但当前脚本做不到；
- 需要视觉校验但环境没有 Word/LibreOffice。

## 7. 不要做

- 不要 `Document()` 新建再拼正文；
- 不要 `paragraph.text = ...`；
- 不要整份 XML 重写；
- 不要 Pandoc 往返；
- 不要用 `fs_write` 直接覆盖原件。
