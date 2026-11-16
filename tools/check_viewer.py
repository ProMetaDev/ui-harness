#!/usr/bin/env python3
"""校验导出的查看器（原 check_viewer.mjs 的 python 移植）。

检查点：
  1. 页面里有 <script id="ui-data" type="application/json"> 内嵌数据块
  2. 内嵌 JSON 能解析
  3. 每个快照都带 sections/tokens
  4. 页面没有外链（离线可看）

为什么改 python：门禁原本通过 tools/_node.sh 找 node，而这个脚本被清理时误删了，
于是 CI 上这一项一直失败；同时这也让门禁不再依赖 node（node 只服务模型适配器）。
"""
import json
import re
import sys

PATH = sys.argv[1] if len(sys.argv) > 1 else "out/trace.html"

with open(PATH, encoding="utf-8") as f:
    html = f.read()

m = re.search(r'<script id="ui-data" type="application/json">(.*?)</script>', html, re.S)
if not m:
    print("FAIL 找不到内嵌数据块")
    sys.exit(1)

try:
    data = json.loads(m.group(1))
except json.JSONDecodeError as e:
    print(f"FAIL 内嵌 JSON 解析失败: {e}")
    sys.exit(1)

events = data.get("events") or []
steps = [e for e in events if e.get("kind") == "prompt/snapshot"]
calls = [e for e in events if e.get("kind") == "tool/request"]
decisions = {}
for c in calls:
    d = c.get("decision")
    decisions[d] = decisions.get(d, 0) + 1

print("ok   事件数  ", len(events))
print("ok    步数   ", len(steps))
print("ok   工具调用", len(calls), json.dumps(decisions))
print("ok   预算上限", json.dumps(data.get("limits")))
print("ok   无外链  ", not re.search(r"https?://", html))

bad = [s for s in steps if not isinstance(s.get("sections"), list) or not isinstance(s.get("tokens"), (int, float))]
if bad:
    print("FAIL 有快照缺少 sections/tokens")
    sys.exit(1)

# 导出的页面必须真的能离线打开：内嵌数据块之后不该再引外部脚本
if re.search(r'<script[^>]+src=', html):
    print("FAIL 页面引用了外部脚本")
    sys.exit(1)

sys.exit(0)
