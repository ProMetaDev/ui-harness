import json
import sys

path = sys.argv[1]
for line in open(path, encoding="utf-8"):
    ev = json.loads(line)
    if ev.get("kind") == "tool/request":
        print(json.dumps(
            {
                "seq": ev.get("seq"),
                "tool": ev.get("tool"),
                "effects": ev.get("effects"),
                "decision": ev.get("decision"),
                "reason": ev.get("reason"),
            },
            ensure_ascii=False,
        ))
        break
