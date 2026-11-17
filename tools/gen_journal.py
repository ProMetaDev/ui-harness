#!/usr/bin/env python3
"""生成一份**哈希链合法**的日志，用来测大日志下的性能。

格式（与内核一致）：
  每行 = 紧凑 JSON，最后一个字段是 hash；
  hash = sha256( 本行去掉末尾 `,"hash":"..."` 之后 + "}" 的字节 )

用法：python3 tools/gen_journal.py <条数> <输出路径> [turn 占比]
"""
import hashlib
import json
import sys
import time

GENESIS = "0" * 64


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 1
    total = int(sys.argv[1])
    path = sys.argv[2]
    turn_every = int(sys.argv[3]) if len(sys.argv) > 3 else 100

    prev = GENESIS
    now = int(time.time() * 1000)
    with open(path, "w", encoding="utf-8") as f:
        for i in range(1, total + 1):
            if i % turn_every == 0:
                kind, payload = "turn/start", {"turn": i // turn_every, "prompt": "bench"}
            else:
                kind, payload = "note", {"i": i}
            event = {"seq": i, "ts": now + i, "kind": kind, **payload, "w": "gen", "prev": prev}
            body = json.dumps(event, ensure_ascii=False, separators=(",", ":"))
            prefix = body[: body.rfind("}")]
            digest = hashlib.sha256((prefix + "}").encode("utf-8")).hexdigest()
            line = prefix + ',"hash":"' + digest + '"}\n'
            f.write(line)
            prev = digest
    print(f"{path}: {total} 条")
    return 0


if __name__ == "__main__":
    sys.exit(main())
