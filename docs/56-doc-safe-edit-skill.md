# 56 · 文档安全编辑 Skill：技术调研

> 目标：AI 修改文档时**不破坏原结构**，先复制工作副本，再最小编辑，最后做结构校验。
> Skill 本体：`demo/skills/doc-safe-edit/SKILL.md`。

## 1. 问题不是“写得好不好”，而是“改完还像不像原文档”

常见破坏方式：

- 整篇 `fs_write` 重写 Markdown，顺手重排表格、改标题层级、丢掉 frontmatter；
- DOCX 用 `Document()` 重建，样式、编号、页眉页脚、书签全丢；
- PPTX 删掉原 slide 再新建，母版/版式/动画丢失；
- XLSX 把公式读成值再写回，公式、样式、合并单元格、图表丢失；
- Pandoc 往返：DOCX → Markdown → DOCX，看起来内容还在，版式和结构已经变了；
- 直接改 PDF 文本流，版面错位。

所以 skill 的第一原则不是“编辑”，是**copy-on-write**：
原件只读，修改只发生在副本；原件哈希在交付前必须不变。

## 2. 方案对比

| 方案 | 适用 | 保真度 | 风险 | 依赖 |
|---|---|---|---|---|
| **复制 + 最小 patch** | Markdown / 文本 / HTML | 高 | 低 | 无 |
| **复制 + OOXML/ODT XML 编辑** | DOCX / PPTX / XLSX / ODT | 最高 | 中：XML 细节多 | zip + XML |
| **复制 + 格式 SDK** | DOCX / PPTX / XLSX | 中高 | 中：SDK 可能丢不支持的元素 | python-docx / python-pptx / openpyxl |
| **复制 + LibreOffice headless/UNO** | Office 全格式 | 高 | 高：重依赖、启动慢、转换有差异 | LibreOffice |
| **Pandoc 往返** | 纯文本转换 | 低 | 高：样式/结构不可逆 | Pandoc |
| **PDF 覆盖层/批注** | PDF | 版面不变 | 低 | PyMuPDF / qpdf |
| **整篇重写** | 仅新建文档 | 不适用 | 最高：必然丢结构 | 无 |

### 推荐策略

1. **Markdown / 文本 / HTML**：复制 + 原文局部 `patch`，不经过格式化器；
2. **DOCX / PPTX / XLSX**：复制 + 优先改已有 XML 节点；复杂编辑再用 SDK，保存后做结构校验；
3. **PDF**：复制 + overlay/annotation，不重排；
4. **任何需要“整篇重写”的情况**：停下来，先问用户。

## 3. 格式细节

### Markdown / 文本

- 用 `patch` 而不是 `fs_write`；只改最小 hunk；
- 保留 frontmatter、标题层级、列表符号、表格对齐、代码围栏、链接引用；
- 保留 BOM、CRLF/LF、末尾换行；
- diff 里出现大面积“格式变化”就是失败信号。

### DOCX（OOXML）

OOXML 是 zip + XML。最稳的做法是复制后只改 `word/document.xml` 中已有的 `<w:t>` 文本节点，保留 `<w:r>`、`<w:p>`、样式、书签、域代码。

注意：

- 一个词可能被拆在多个 `<w:r>`/`<w:t>` 里，不能假设“段落文本是一个字符串”；
- `python-docx` 适合常规段落/表格编辑，但它不保证保留所有 unsupported OOXML；
- 不要用 `Document()` 新建再拼正文；
- 保存后 `unzip -t`，并比对段落/表格/图片数量。

参考实现：[Editing existing DOCX via OOXML](https://github.com/openkursar/hello-halo/blob/a1b6d395e9c3609d33cefa0fd2fb99d280bfb4e0/resources/builtin-skills/docx/references/editing.md)、[docx-official/edit.md](https://github.com/xiaomimimo/mimo-code/blob/5a538cb4f97b9c4c8eb09a845ec06c1e5f1f3e8f/packages/cli/src/skill/builtin/.bundle/docx-official/edit.md)。

### PPTX

- 只改已有 shape 的 text frame；
- 保留 slide、layout、master、theme、图片关系；
- 不要“删 slide 再新建”来替换内容；
- 保存后核对 slide 数、图片数、占位符数。

参考：[python-pptx](https://python-pptx.readthedocs.io/)。

### XLSX

- 只改目标单元格；公式保持公式；
- 不要把整表读成值再写回；
- 保存后核对 sheet 数、公式数、合并单元格、图表；
- openpyxl 对部分高级特性（透视表、切片器、部分图表）不保证完整保留；这类文件优先 LibreOffice 或直接 XML。

参考：[openpyxl](https://openpyxl.readthedocs.io/)。

### PDF

- PDF 不是流式文档；安全操作是 overlay、批注、redaction；
- 重排文字会破坏原版面；
- 复制后操作副本，保留原文件。

### HTML

- 用 DOM/字符串局部修改；序列化可能改变属性顺序或自闭合写法；
- 改完做一次文本 diff，避免整页重排。

## 4. 校验闸门

skill 要求每次编辑后至少过四关：

1. **原件哈希未变**：`sha256(original)` 与编辑前一致；
2. **副本可打开**：`unzip -t`（Office）、XML 可解析、PDF 页数可读；
3. **结构守恒**：标题/表格/图片/代码块，或 DOCX 段落/表格/图片、PPTX slides、XLSX sheets/formulas，不减少；
4. **diff 只含预期改动**：文本 diff 不出现大面积格式变化。

失败就保留原件、停止交付；不“修到能跑”。

## 5. Skill 的边界

`doc-safe-edit` 是**操作规约**，不是新的文件格式引擎：

- 不替代 `patch`/`fs_edit`；
- 不内置 Office 解析器；
- 不默认安装 LibreOffice/Python 包；
- 不在用户未确认时替换原件；
- 不把“复制副本”变成“生成一个看起来像原件的重写版”。

它解决的是行为约束：**先复制、只改局部、改完校验、默认不覆盖**。

## 6. 验收

- [ ] Skill 在 `<workspace>/skills/doc-safe-edit/SKILL.md` 被 `ui skills` 发现；
- [ ] Markdown 编辑后标题/表格/代码块数量不变；
- [ ] DOCX 副本 `unzip -t` 通过，段落/表格/图片数不减少；
- [ ] PPTX slide 数、XLSX sheet/公式数不减少；
- [ ] 原件 sha256 在交付时未变；
- [ ] 用户说“替换”之前，原件没有被覆盖。

## 7. 后续可选增强

1. 提供 `scripts/doc_guard.py`：哈希、zip 完整性、结构计数、diff 摘要一条命令；
2. 对 DOCX 做 `w:r` 级文本替换，避免 SDK 重建；
3. 用 LibreOffice 转 PDF/PNG 做视觉抽查；
4. 把“原件未变/结构守恒”写进 `ui verify` 的审计事件。
