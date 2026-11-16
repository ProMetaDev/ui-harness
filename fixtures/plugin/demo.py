import json
import sys

req = json.load(sys.stdin)
tool = req.get("tool")
args = req.get("args") or {}
if tool == "echo":
    print("echo: " + str(args.get("text", "")))
elif tool == "write":
    path = args.get("path", "out.txt")
    with open(path, "w") as f:
        f.write(str(args.get("content", "")))
    print("wrote " + path)
else:
    sys.exit(3)
