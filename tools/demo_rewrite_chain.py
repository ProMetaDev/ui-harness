#!/usr/bin/env python3
"""演示：单机哈希链挡不住「改完重算哈希」的攻击者。

不做任何签名/外部锚定，所以攻击者从改动点往后重算，就能造出一条**自洽**的链。
这个脚本用 python 独立实现链规则（不依赖 Rust 代码），证明外行也能做到。

用法：python3 tools/demo_rewrite_chain.py <journal.jsonl>
退出码：0 = 伪造成功且 verify 仍通过（说明限制真实存在）；1 = 被检出（说明我们比预期更强）
"""
import hashlib
import json
import subprocess
import sys

path = sys.argv[1]
lines = [l for l in open(path, encoding="utf-8").read().splitlines() if l.strip()]


def hash_of(line: str) -> str:
    """hash = sha256(该行去掉尾部 ,"hash":"…" 后**补回 `}`** 的字节)

    注意补 `}`：原始 append 是先序列化完整对象（含 `}`），再插入 `,"hash":"…"`。
    少了这个 `}` 就会算出不同的值——我第一次演示就栽在这里。
    """
    marker = ',"hash":"'
    idx = line.rfind(marker)
    return hashlib.sha256((line[:idx] + "}").encode()).hexdigest()


# 1) 找到第 10 条，改掉它 payload 里的一个值
target = min(9, len(lines) - 1)
event = json.loads(lines[target])
event["forged"] = True  # 事件是扁平的（kind + 字段），不是 {kind,payload}
lines[target] = json.dumps(event, ensure_ascii=False, separators=(",", ":"))

# 2) 从改动点往后重算 prev 与 hash
prev = json.loads(lines[target - 1])["hash"] if target > 0 else "0" * 64
for i in range(target, len(lines)):
    e = json.loads(lines[i])
    e.pop("hash", None)  # 必须先去掉旧 hash，否则前缀里会残留一个 hash 字段
    e["prev"] = prev
    body = json.dumps(e, ensure_ascii=False, separators=(",", ":")).rstrip("}")
    line = body + ',"hash":"' + "0" * 64 + '"}'
    h = hash_of(line)
    e["hash"] = h
    line = body + ',"hash":"' + h + '"}'
    lines[i] = line
    prev = h

open(path, "w", encoding="utf-8").write("\n".join(lines) + "\n")
print(f"已修改第 {target + 1} 条并重算其后 {len(lines) - target} 条的哈希")

binary = sys.argv[2] if len(sys.argv) > 2 else "ui"
out = subprocess.run([binary, "verify", "--journal", path], capture_output=True, text=True)
print("verify 退出码:", out.returncode)
print((out.stdout + out.stderr).strip().splitlines()[0] if (out.stdout or out.stderr) else "")

if out.returncode == 0:
    print("结论：伪造未被检出 —— 「自洽 ≠ 真实」得证（需要外部锚定才能挡住）")
    sys.exit(0)
print("结论：被检出 —— 我们的校验比预期更强（那这条限制可以放宽）")
sys.exit(1)
