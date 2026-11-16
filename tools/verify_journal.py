#!/usr/bin/env python3
"""独立日志校验器：不依赖 ui 的 Rust 代码，只依赖这份格式说明。

格式（每行一条记录）：
  {"seq":N,"ts":ms,"kind":"...",<payload...>,"w":"写者","prev":"64hex","hash":"64hex"}

规则：
  1. hash = sha256( 本行去掉最后的 ,"hash":"..." 之后的全部字节 )
     —— 注意是**按字节**切掉尾字段，不是重新序列化：键序必须保持原样，
        任何"解析再 dumps"都会改变字节，导致永远校验不过。
  2. 第 1 条的 prev 全 0；之后每条 prev == 上一条的 hash。

用法：python3 tools/verify_journal.py <journal.jsonl>
退出码：0 完好 / 2 链断或被改 / 3 尾部残缺（可修复）
"""
import hashlib
import json
import re
import sys

HASH_TAIL = re.compile(r',"hash":"([0-9a-f]{64})"\}$')
ZERO = "0" * 64


def main(path: str) -> int:
    with open(path, "rb") as f:
        raw = f.read()

    # 文件可能不是以换行结尾（崩溃）；最后一段单独判断
    text = raw.decode("utf-8", errors="replace")
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()

    prev = ZERO
    count = 0
    torn = 0

    for i, line in enumerate(lines, start=1):
        if not line.strip():
            continue
        m = HASH_TAIL.search(line)
        if not m:
            # 不是合法记录：可能是尾部撕裂
            torn += 1
            print(f"  第 {i} 行不是完整记录（长度 {len(line)}）")
            continue
        want = m.group(1)
        preimage = line[: m.start()] + "}"
        got = hashlib.sha256(preimage.encode("utf-8")).hexdigest()
        if got != want:
            print(f"  第 {i} 行哈希不符：记录 {want[:12]}… 实际 {got[:12]}…")
            return 2
        try:
            rec = json.loads(line)
        except json.JSONDecodeError as e:
            print(f"  第 {i} 行不是 JSON：{e}")
            return 2
        if rec.get("prev") != prev:
            print(f"  第 {i} 行链接断裂：prev {str(rec.get('prev'))[:12]}… 期望 {prev[:12]}…")
            return 2
        prev = want
        count += 1

    print(f"记录 {count} 条 · 链{'完整' if torn == 0 else '尾部残缺'}")
    if torn:
        print(f"  尾部有 {torn} 行不完整（崩溃残留，可修复）")
        return 3
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("用法：python3 tools/verify_journal.py <journal.jsonl>")
        sys.exit(1)
    sys.exit(main(sys.argv[1]))
