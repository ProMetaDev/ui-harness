#!/usr/bin/env python3
"""DOCX copy-on-write guard.

Commands:
  inspect FILE
  copy FILE [--out OUT]
  replace FILE --find TEXT --replace TEXT [--all] [--dry-run]
  validate ORIGINAL COPY

`replace` edits FILE in place (intended to be an `.edit-*` copy), but writes
through a temp file and never touches the original path passed to `copy`.
Only word/document.xml is changed; every other zip part is copied byte-for-byte.
Requires lxml (for namespace-preserving XML edits).
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
import tempfile
import zipfile
from datetime import datetime
from pathlib import Path

W = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"
REL = "{http://schemas.openxmlformats.org/package/2006/relationships}"
CT = "{http://schemas.openxmlformats.org/package/2006/content-types}"


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def die(msg, code=1):
    print("ERROR: " + msg, file=sys.stderr)
    raise SystemExit(code)


def load_lxml():
    try:
        from lxml import etree
        return etree
    except ImportError:
        die("需要 lxml：pip install lxml；没有它不要直接改 OOXML")


def parse_part(zf, name):
    etree = load_lxml()
    data = zf.read(name)
    parser = etree.XMLParser(remove_blank_text=False, recover=False,
                             resolve_entities=False, no_network=True)
    return etree.fromstring(data, parser)


def text_nodes(paragraph):
    return list(paragraph.iter(W + "t"))


def paragraph_text(paragraph):
    return "".join(n.text or "" for n in text_nodes(paragraph))


def structure(path):
    etree = load_lxml()
    with zipfile.ZipFile(path) as zf:
        bad = zf.testzip()
        names = set(zf.namelist())
        doc = parse_part(zf, "word/document.xml")
        body = doc.find(".//" + W + "body")
        paras = list(body.iter(W + "p")) if body is not None else []
        tables = list(body.iter(W + "tbl")) if body is not None else []
        images = list(body.iter(W + "drawing")) + list(body.iter(W + "pict"))
        sections = list(doc.iter(W + "sectPr"))
        text = "".join(n.text or "" for n in doc.iter(W + "t"))
        rels = 0
        missing_targets = []
        for rel_name in [n for n in names if n.endswith(".rels")]:
            try:
                rel = parse_part(zf, rel_name)
            except Exception:
                continue
            base = rel_name.rsplit("_rels/", 1)[0]
            for r in rel.iter(REL + "Relationship"):
                rels += 1
                if r.get("TargetMode") == "External":
                    continue
                raw = (r.get("Target") or "").replace("\\", "/")
                target = raw[1:] if raw.startswith("/") else base + raw
                target = os.path.normpath(target).replace("\\", "/")
                if target not in names:
                    missing_targets.append(target)
        headers = [n for n in names if re.match(r"word/header\d+\.xml$", n)]
        footers = [n for n in names if re.match(r"word/footer\d+\.xml$", n)]
        media = [n for n in names if n.startswith("word/media/")]
        shapes = [table_shape(t) for t in tables]
        return {
            "ok": bad is None and not missing_targets,
            "zip_error": bad,
            "missing_rel_targets": sorted(set(missing_targets)),
            "parts": len(names),
            "paragraphs": len(paras),
            "tables": len(tables),
            "table_rows": sum(s["rows"] for s in shapes),
            "table_cells": sum(s["cells"] for s in shapes),
            "table_grid_spans": sum(s["grid_spans"] for s in shapes),
            "table_vmerge": sum(s["vmerge"] for s in shapes),
            "images": len(images),
            "sections": len(sections),
            "headers": len(headers),
            "footers": len(footers),
            "media": len(media),
            "relationships": rels,
            "text_chars": len(text),
            "has_styles": "word/styles.xml" in names,
            "has_numbering": "word/numbering.xml" in names,
        }


def inspect(path):
    st = structure(path)
    st["path"] = str(Path(path).resolve())
    st["bytes"] = os.path.getsize(path)
    st["sha256"] = sha256(path)
    return st


def cmd_inspect(args):
    print(json.dumps(inspect(args.file), ensure_ascii=False, indent=2))
    return 0


def cmd_copy(args):
    src = Path(args.file)
    if not src.is_file():
        die(f"文件不存在: {src}")
    if args.out:
        dst = Path(args.out)
    else:
        stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        dst = src.with_name(f"{src.stem}.edit-{stamp}{src.suffix}")
    if dst.exists():
        die(f"目标已存在，拒绝覆盖: {dst}")
    shutil.copy2(src, dst)
    if sha256(src) != sha256(dst):
        die("复制后哈希不一致")
    print(json.dumps({
        "original": str(src.resolve()),
        "copy": str(dst.resolve()),
        "original_sha256": sha256(src),
        "bytes": dst.stat().st_size,
    }, ensure_ascii=False, indent=2))
    return 0


def replace_in_paragraph(p, find, repl, do_all):
    nodes = [n for n in text_nodes(p)]
    if not nodes:
        return 0
    original = "".join(n.text or "" for n in nodes)
    positions = []
    start = 0
    while True:
        at = original.find(find, start)
        if at < 0:
            break
        positions.append(at)
        if not do_all:
            break
        start = at + max(1, len(find))
    if not positions:
        return 0
    # Apply right-to-left so earlier offsets stay valid.
    for pos in reversed(positions):
        end = pos + len(find)
        cursor = 0
        for n in nodes:
            t = n.text or ""
            n_start, n_end = cursor, cursor + len(t)
            cursor = n_end
            if n_end <= pos or n_start >= end:
                continue
            local_start = max(0, pos - n_start)
            local_end = min(len(t), end - n_start)
            before = t[:local_start]
            after = t[local_end:]
            if n_start <= pos and n_end >= pos:
                text = before + repl
                if n_end >= end:
                    text += after
            elif n_end >= end:
                text = after
            else:
                text = ""
            n.text = text
            if text[:1].isspace() or text[-1:].isspace():
                n.set("{http://www.w3.org/XML/1998/namespace}space", "preserve")
    return len(positions)


def rewrite_doc(src, dst, mutate):
    etree = load_lxml()
    with zipfile.ZipFile(src) as zin:
        doc = parse_part(zin, "word/document.xml")
        result = mutate(doc)
        if not result:
            die("没有发生任何改动；未写入任何文件")
        data = etree.tostring(doc, xml_declaration=True, encoding="UTF-8", standalone=None)
        with zipfile.ZipFile(dst, "w") as zout:
            for info in zin.infolist():
                if info.filename == "word/document.xml":
                    zi = zipfile.ZipInfo(info.filename, date_time=info.date_time)
                    zi.compress_type = info.compress_type
                    zi.external_attr = info.external_attr
                    zout.writestr(zi, data)
                else:
                    zout.writestr(info, zin.read(info.filename))
    return result


def rewrite_document(src, dst, find, repl, do_all):
    def mutate(doc):
        total = 0
        for p in doc.iter(W + "p"):
            total += replace_in_paragraph(p, find, repl, do_all)
        return total
    return rewrite_doc(src, dst, mutate)


def edit_atomic(target, mutate):
    tmpdir = target.parent / ".ui" / "tmp"
    tmpdir.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix="docx-safe-", suffix=".docx", dir=str(tmpdir))
    os.close(fd)
    try:
        result = rewrite_doc(target, tmp, mutate)
        os.replace(tmp, target)
        return result
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def table_rows(tbl):
    return tbl.findall(W + "tr")


def cell_text(tc):
    return "".join(t.text or "" for t in tc.iter(W + "t"))


def cell_grid(tc):
    span = 1
    tcpr = tc.find(W + "tcPr")
    if tcpr is not None:
        gs = tcpr.find(W + "gridSpan")
        if gs is not None:
            try:
                span = max(1, int(gs.get(W + "val") or "1"))
            except ValueError:
                span = 1
    return span


def cell_is_vmerge_continue(tc):
    tcpr = tc.find(W + "tcPr")
    if tcpr is None:
        return False
    vm = tcpr.find(W + "vMerge")
    return vm is not None and (vm.get(W + "val") or "continue") == "continue"


def table_shape(tbl):
    rows = table_rows(tbl)
    cells = sum(len(r.findall(W + "tc")) for r in rows)
    spans = sum(1 for r in rows for tc in r.findall(W + "tc") if cell_grid(tc) > 1)
    merges = sum(1 for r in rows for tc in r.findall(W + "tc") if cell_is_vmerge_continue(tc))
    return {"rows": len(rows), "cells": cells, "grid_spans": spans, "vmerge": merges}


def find_table(doc, index):
    tables = list(doc.iter(W + "tbl"))
    if index < 1 or index > len(tables):
        die(f"表格序号 {index} 不存在（共 {len(tables)} 个）")
    return tables[index - 1]


def find_cell(tbl, row, col):
    rows = table_rows(tbl)
    if row < 1 or row > len(rows):
        die(f"行 {row} 越界（共 {len(rows)} 行）")
    logical = 0
    for tc in rows[row - 1].findall(W + "tc"):
        span = cell_grid(tc)
        if logical + 1 <= col <= logical + span:
            if cell_is_vmerge_continue(tc):
                die(f"单元格 ({row},{col}) 是纵向合并的延续格；请填合并首格")
            return tc
        logical += span
    die(f"列 {col} 越界（第 {row} 行逻辑列数 {logical}）")


def first_run(tc):
    for r in tc.iter(W + "r"):
        return r
    return None


def fill_cell(tc, value, overwrite, template_run=None):
    ps = tc.findall(W + "p")
    if not ps:
        etree = load_lxml()
        p = etree.SubElement(tc, W + "p")
        ps = [p]
    p = ps[0]
    old = cell_text(tc).strip()
    placeholder = bool(re.fullmatch(r"\s*(\{\{.*?\}\}|\$\{.*?\}|_{2,}|待填|TODO)\s*", old))
    if old and not placeholder and not overwrite:
        die(f"目标单元格已有内容（{old!r}）；默认不覆盖，确需覆盖请加 --overwrite")
    if len(ps) > 1 and old:
        die("目标单元格是多段落内容；脚本不支持，避免只改第一段造成半覆盖")
    # 保留段落属性；清掉旧 run/hyperlink，避免 text 叠加。
    existing_run = first_run(tc)
    for child in list(p):
        if child.tag in (W + "r", W + "hyperlink"):
            p.remove(child)
    etree = load_lxml()
    r = etree.SubElement(p, W + "r")
    src_run = existing_run if (existing_run is not None and existing_run.find(W + "rPr") is not None) else template_run
    if src_run is not None and src_run.find(W + "rPr") is not None:
        from copy import deepcopy
        r.append(deepcopy(src_run.find(W + "rPr")))
    parts = str(value).split("\n")
    t = etree.SubElement(r, W + "t")
    t.text = parts[0]
    if parts[0][:1].isspace() or parts[0][-1:].isspace():
        t.set("{http://www.w3.org/XML/1998/namespace}space", "preserve")
    for part in parts[1:]:
        etree.SubElement(r, W + "br")
        t = etree.SubElement(r, W + "t")
        t.text = part
    return {"old": old, "new": str(value), "placeholder": placeholder}


def table_fill(doc, index, row, col, value, overwrite):
    tbl = find_table(doc, index)
    tc = find_cell(tbl, row, col)
    # 空单元格没有 run，格式模板按优先级取：
    # 同行标签格 → 同列表头 → 无。
    template = None
    for tr, tcol in ((row, 1), (1, col)):
        try:
            candidate = first_run(find_cell(tbl, tr, tcol))
        except SystemExit:
            candidate = None
        if candidate is not None:
            template = candidate
            break
    info = fill_cell(tc, value, overwrite, template)
    info.update({"table": index, "row": row, "col": col})
    return info


def cmd_replace(args):
    target = Path(args.file)
    if not target.is_file():
        die(f"文件不存在: {target}")
    if args.find == "":
        die("--find 不能为空")
    # Refuse to edit an obvious original if a sibling edit copy exists.
    if ".edit" not in target.name:
        print("WARN: 目标文件名没有 .edit 标记；确认这是副本，不是原件", file=sys.stderr)
    if args.dry_run:
        with zipfile.ZipFile(target) as zf:
            doc = parse_part(zf, "word/document.xml")
            hits = sum(paragraph_text(p).count(args.find) for p in doc.iter(W + "p"))
        print(json.dumps({"dryRun": True, "matches": hits, "find": args.find}, ensure_ascii=False))
        return 0 if hits else 1
    def mutate(doc):
        total = 0
        for p in doc.iter(W + "p"):
            total += replace_in_paragraph(p, args.find, args.replace, args.all)
        return total

    count = edit_atomic(target, mutate)
    print(json.dumps({"replaced": count, "file": str(target.resolve())}, ensure_ascii=False))
    return 0


def cmd_table_fill(args):
    target = Path(args.file)
    if not target.is_file():
        die(f"文件不存在: {target}")
    if args.value is None:
        die("--value 不能为空")
    if ".edit" not in target.name:
        print("WARN: 目标文件名没有 .edit 标记；确认这是副本，不是原件", file=sys.stderr)

    def mutate(doc):
        return table_fill(doc, args.table, args.row, args.col, args.value, args.overwrite)

    info = edit_atomic(target, mutate)
    info["file"] = str(target.resolve())
    print(json.dumps(info, ensure_ascii=False, indent=2))
    return 0


def cmd_table_fill_map(args):
    target = Path(args.file)
    if not target.is_file():
        die(f"文件不存在: {target}")
    if ".edit" not in target.name:
        print("WARN: 目标文件名没有 .edit 标记；确认这是副本，不是原件", file=sys.stderr)
    try:
        plan = json.loads(Path(args.map).read_text(encoding="utf-8"))
    except Exception as e:
        die(f"读不懂 --map JSON: {e}")
    table = int(plan.get("table", 1))
    fills = plan.get("fills") or plan.get("fields") or []
    if not fills:
        die("--map 里没有 fills")

    def mutate(doc):
        results = []
        for f in fills:
            try:
                row = int(f["row"])
                col = int(f["col"])
                value = str(f["value"])
            except (KeyError, TypeError, ValueError):
                die(f"fill 条目缺少 row/col/value: {f!r}")
            info = table_fill(doc, table, row, col, value, bool(f.get("overwrite")))
            results.append(info)
        return results

    results = edit_atomic(target, mutate)
    print(json.dumps({
        "table": table,
        "fills": results,
        "file": str(target.resolve()),
    }, ensure_ascii=False, indent=2))
    return 0


def cmd_validate(args):
    a = inspect(args.original)
    b = inspect(args.copy)
    keys = ["paragraphs", "tables", "table_rows", "table_cells", "table_grid_spans",
            "table_vmerge", "images", "sections", "headers", "footers",
            "media", "relationships", "has_styles", "has_numbering"]
    lost = [k for k in keys if isinstance(a[k], int) and b[k] < a[k]]
    changed = {k: [a[k], b[k]] for k in keys if a[k] != b[k]}
    report = {
        "ok": b["ok"] and not lost and a["sha256"] != b["sha256"],
        "original_sha256": a["sha256"],
        "copy_sha256": b["sha256"],
        "original_untouched": a["sha256"] == sha256(args.original),
        "zip_ok": b["ok"],
        "lost_structure": lost,
        "changed": changed,
        "text_chars": [a["text_chars"], b["text_chars"]],
    }
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if report["ok"] else 1


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("inspect")
    p.add_argument("file")
    p.set_defaults(fn=cmd_inspect)

    p = sub.add_parser("copy")
    p.add_argument("file")
    p.add_argument("--out")
    p.set_defaults(fn=cmd_copy)

    p = sub.add_parser("replace")
    p.add_argument("file")
    p.add_argument("--find", required=True)
    p.add_argument("--replace", required=True)
    p.add_argument("--all", action="store_true")
    p.add_argument("--dry-run", action="store_true")
    p.set_defaults(fn=cmd_replace)

    p = sub.add_parser("table-fill")
    p.add_argument("file")
    p.add_argument("--table", type=int, default=1, help="第几个表格（1 起）")
    p.add_argument("--row", type=int, required=True, help="第几行（1 起，含表头）")
    p.add_argument("--col", type=int, required=True, help="第几个逻辑列（1 起，跨列表格按逻辑列）")
    p.add_argument("--value", required=True)
    p.add_argument("--overwrite", action="store_true", help="允许覆盖已有内容；默认只填空/占位符")
    p.set_defaults(fn=cmd_table_fill)

    p = sub.add_parser("table-fill-map")
    p.add_argument("file")
    p.add_argument("--map", required=True, help="JSON：{table, fills:[{row,col,value}]}")
    p.set_defaults(fn=cmd_table_fill_map)

    p = sub.add_parser("validate")
    p.add_argument("original")
    p.add_argument("copy")
    p.set_defaults(fn=cmd_validate)

    args = ap.parse_args()
    return args.fn(args)


if __name__ == "__main__":
    raise SystemExit(main())
