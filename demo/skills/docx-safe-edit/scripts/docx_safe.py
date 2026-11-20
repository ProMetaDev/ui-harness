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
        return {
            "ok": bad is None and not missing_targets,
            "zip_error": bad,
            "missing_rel_targets": sorted(set(missing_targets)),
            "parts": len(names),
            "paragraphs": len(paras),
            "tables": len(tables),
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


def rewrite_document(src, dst, find, repl, do_all):
    etree = load_lxml()
    with zipfile.ZipFile(src) as zin:
        doc = parse_part(zin, "word/document.xml")
        total = 0
        for p in doc.iter(W + "p"):
            total += replace_in_paragraph(p, find, repl, do_all)
        if total == 0:
            die("没有找到要替换的文本；未写入任何文件")
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
    return total


def cmd_replace(args):
    target = Path(args.file)
    if not target.is_file():
        die(f"文件不存在: {target}")
    if args.find == "":
        die("--find 不能为空")
    # Refuse to edit an obvious original if a sibling edit copy exists.
    if ".edit-" not in target.name:
        print("WARN: 目标文件名没有 .edit- 标记；确认这是副本，不是原件", file=sys.stderr)
    if args.dry_run:
        with zipfile.ZipFile(target) as zf:
            doc = parse_part(zf, "word/document.xml")
            hits = sum(paragraph_text(p).count(args.find) for p in doc.iter(W + "p"))
        print(json.dumps({"dryRun": True, "matches": hits, "find": args.find}, ensure_ascii=False))
        return 0 if hits else 1
    fd, tmp = tempfile.mkstemp(prefix="docx-safe-", suffix=".docx", dir=str(target.parent))
    os.close(fd)
    try:
        count = rewrite_document(target, tmp, args.find, args.replace, args.all)
        os.replace(tmp, target)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
    print(json.dumps({"replaced": count, "file": str(target.resolve())}, ensure_ascii=False))
    return 0


def cmd_validate(args):
    a = inspect(args.original)
    b = inspect(args.copy)
    keys = ["paragraphs", "tables", "images", "sections", "headers", "footers",
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

    p = sub.add_parser("validate")
    p.add_argument("original")
    p.add_argument("copy")
    p.set_defaults(fn=cmd_validate)

    args = ap.parse_args()
    return args.fn(args)


if __name__ == "__main__":
    raise SystemExit(main())
