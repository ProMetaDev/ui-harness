#!/usr/bin/env bash
# 对抗检查：journal、SSE 与可分享 /snapshot 都不能带明文。
#
# 除了完整 shell 输出，还让“模型进程”把 AWS 形状与 PEM 私钥故意拆成多个 delta。
# 只 grep 完整密钥是不够的：旧实现逐片脱敏时，每一片都不命中，浏览器却能拼回原文。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

WS=/tmp/ui-web-leak
PORT=$(ui_free_port)
rm -rf "$WS"; mkdir -p "$WS"
CANARY="AKIAWEBLEAKCANARY123456"  # allowlist secret（故意写成密钥形状的测试夹具）
DELTA_CANARY="AKIAWEBDELTACANARY12345"  # allowlist secret（跨 delta 的测试夹具）
PEM_BODY="WEBPEMPRIVATEKEYCANARY"  # allowlist secret（假私钥正文，非真凭据）
SSE="$WS/sse.txt"
BACKLOG="$WS/sse-backlog.txt"
ALL_BACKLOG="$WS/sse-backlog-all.txt"
READY="$WS/ready.json"
META="$WS/meta.json"
SNAPSHOT="$WS/snapshot.html"
JOURNAL="$WS/.ui/journal.jsonl"
SSE_PID=""

cat > "$WS/model.py" <<'PY'
import json
import os
import pathlib
import sys

json.load(sys.stdin)
root = pathlib.Path(__file__).resolve().parent
counter = root / "model-calls.txt"
try:
    call = int(counter.read_text().strip())
except (OSError, ValueError):
    call = 0
counter.write_text(str(call + 1))

if call == 0:
    body = os.environ["WEB_PEM_BODY"]
    pem = "-----BEGIN RSA PRIVATE KEY-----\n" + body + "\n-----END RSA PRIVATE KEY-----"  # allowlist secret（假私钥块）
    streamed = ("before " + os.environ["WEB_DELTA_CANARY"]
                + " middle\n" + pem + "\nafter")
    final = {
        "content": "",
        "toolCalls": [{
            "name": "shell",
            "args": {"command": "echo " + os.environ["WEB_SHELL_CANARY"]},
        }],
    }
else:
    streamed = "done"
    final = {"content": "done " + os.environ["WEB_DELTA_CANARY"], "toolCalls": []}

# 5 字节分片：前缀、PEM 头与正文都跨事件，逼出“逐片脱敏”的漏洞。
for start in range(0, len(streamed), 5):
    print(json.dumps({"delta": streamed[start:start + 5]}), flush=True)
print(json.dumps(final), flush=True)
PY

UI_MODEL_CMD="python3 '$WS/model.py'" \
WEB_SHELL_CANARY="$CANARY" \
WEB_DELTA_CANARY="$DELTA_CANARY" \
WEB_PEM_BODY="$PEM_BODY" \
"$UI_BIN" serve --workspace "$WS" --port "$PORT" --yes --token off \
  >"$WS/server.log" 2>&1 &
PID=$!
cleanup() {
  [ -z "$SSE_PID" ] || kill "$SSE_PID" 2>/dev/null
  kill "$PID" 2>/dev/null
  wait "$PID" 2>/dev/null
}
trap cleanup EXIT

if ! ui_wait_port "$PORT" 15; then
  echo "FAIL 服务起不来（$WS/server.log）"
  exit 1
fi
BASE="http://127.0.0.1:$PORT"

curl -sN --max-time 10 "$BASE/api/events" >"$SSE" 2>/dev/null &
SSE_PID=$!
ready=0
for _ in $(seq 1 50); do
  if grep -q '^: ready$' "$SSE" 2>/dev/null; then ready=1; break; fi
  kill -0 "$SSE_PID" 2>/dev/null || break
  sleep 0.1
done
if [ "$ready" -ne 1 ]; then
  echo "FAIL SSE 未在发任务前确认订阅就绪"
  exit 1
fi
curl -s -X POST "$BASE/api/say" -H 'content-type: application/json' -d '{"text":"跑一下"}' >/dev/null

finished=0
live_delta=0
for _ in $(seq 1 100); do
  if grep -q '"type":"delta"' "$SSE" 2>/dev/null; then live_delta=1; fi
  if [ -f "$JOURNAL" ] && grep -q '"kind":"turn/end"' "$JOURNAL"; then
    finished=1
    break
  fi
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.1
done
if [ "$finished" -ne 1 ]; then
  echo "FAIL 任务没有正常收尾（$WS/server.log）"
  exit 1
fi
sleep 0.2
if grep -q '"type":"delta"' "$SSE" 2>/dev/null; then live_delta=1; fi
if [ "$live_delta" -ne 1 ]; then
  echo "FAIL 已订阅的实时 SSE 没有收到 delta（可能只在 backlog 里）"
  exit 1
fi
MAX_SEQ=$(python3 - "$SSE" <<'PY'
import json
import pathlib
import sys
seqs = []
for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines():
    if line.startswith("data: "):
        try:
            seqs.append(int(json.loads(line[6:]).get("seq", 0)))
        except (ValueError, json.JSONDecodeError):
            pass
print(max(seqs, default=0))
PY
)
if [ "$MAX_SEQ" -lt 2 ]; then
  echo "FAIL 实时 SSE 没有可重连的事件序号"
  exit 1
fi
kill "$SSE_PID" 2>/dev/null
wait "$SSE_PID" 2>/dev/null
SSE_PID=""
# 从最后一个事件的前一序号重连：backlog 必须精确只包含最后一个事件。
curl -sN --max-time 1 "$BASE/api/events?since=$((MAX_SEQ - 1))" >"$BACKLOG" 2>/dev/null || true
curl -sN --max-time 1 "$BASE/api/events?since=0" >"$ALL_BACKLOG" 2>/dev/null || true
curl -s "$BASE/api/ready" >"$READY" 2>/dev/null
curl -s "$BASE/api/meta" >"$META" 2>/dev/null
curl -s "$BASE/snapshot" >"$SNAPSHOT" 2>/dev/null

hits() {
  grep -c "$1" "$2" 2>/dev/null || true
}

fail=0
for secret in "$CANARY" "$DELTA_CANARY" "$PEM_BODY"; do
  journal_hits=$(hits "$secret" "$JOURNAL")
  sse_hits=$(hits "$secret" "$SSE")
  backlog_hits=$(hits "$secret" "$BACKLOG")
  all_backlog_hits=$(hits "$secret" "$ALL_BACKLOG")
  ready_hits=$(hits "$secret" "$READY")
  meta_hits=$(hits "$secret" "$META")
  snap_hits=$(hits "$secret" "$SNAPSHOT")
  journal_hits=${journal_hits:-0}; sse_hits=${sse_hits:-0}; backlog_hits=${backlog_hits:-0}
  all_backlog_hits=${all_backlog_hits:-0}; ready_hits=${ready_hits:-0}; meta_hits=${meta_hits:-0}; snap_hits=${snap_hits:-0}
  echo "明文命中 journal=$journal_hits SSE=$sse_hits backlog=$backlog_hits all-backlog=$all_backlog_hits ready=$ready_hits meta=$meta_hits snapshot=$snap_hits（期望全 0）"
  [ "$journal_hits" -eq 0 ] || { echo "FAIL 日志里有明文"; fail=1; }
  [ "$sse_hits" -eq 0 ]     || { echo "FAIL SSE 里有明文（浏览器 DevTools 就能看到）"; fail=1; }
  [ "$backlog_hits" -eq 0 ] || { echo "FAIL SSE backlog 里有明文"; fail=1; }
  [ "$all_backlog_hits" -eq 0 ] || { echo "FAIL 完整 SSE backlog 里有明文"; fail=1; }
  [ "$ready_hits" -eq 0 ] || { echo "FAIL /api/ready 里有明文"; fail=1; }
  [ "$meta_hits" -eq 0 ] || { echo "FAIL /api/meta 里有明文"; fail=1; }
  [ "$snap_hits" -eq 0 ]    || { echo "FAIL /snapshot 的 HTML 里有明文（这个文件是给人转发的）"; fail=1; }
done

# 完整字符串 grep 抓不到“分片各自不命中、拼接后还原”的旧漏洞，必须按事件重组。
if ! python3 - "$SSE" "$BACKLOG" "$ALL_BACKLOG" "$SNAPSHOT" "$JOURNAL" "$DELTA_CANARY" "$PEM_BODY" "$MAX_SEQ" <<'PY'
import json
import pathlib
import re
import sys

sse_path, backlog_path, all_backlog_path, snapshot_path, journal_path, aws, pem_body, max_seq = sys.argv[1:]
max_seq = int(max_seq)
failures = []
SAFE_NOTICE = "[模型正在生成；正文将在回复完成后安全显示]"


def parse_sse(path):
    events = []
    for line in pathlib.Path(path).read_text(encoding="utf-8").splitlines():
        if line.startswith("data: "):
            try:
                events.append(json.loads(line[6:]))
            except json.JSONDecodeError:
                failures.append(path + " 含无法解析的事件")
    return events


def display_text(events, label):
    deltas = [event for event in events if event.get("type") == "delta"]
    if not deltas:
        failures.append(label + " 没有收到 delta 事件")
    joined = "".join(str(event.get("text", "")) for event in events
                     if event.get("type") in ("delta", "assistant"))
    if aws in joined:
        failures.append(label + " 的 delta/assistant 可拼接还原 AWS 明文")
    if pem_body in joined:
        failures.append(label + " 的 delta/assistant 可拼接还原 PEM 正文")
    if SAFE_NOTICE not in joined:
        failures.append(label + " 没有安全流式提示")
    if "[REDACTED:" not in joined:
        failures.append(label + " 的权威 assistant 没有留下脱敏标记")


sse_events = parse_sse(sse_path)
display_text(sse_events, "SSE")

backlog_events = parse_sse(backlog_path)
if [event.get("seq") for event in backlog_events] != [max_seq]:
    failures.append("since 重连的 backlog 集合不精确")
backlog_text = json.dumps(backlog_events, ensure_ascii=False)
if aws in backlog_text or pem_body in backlog_text:
    failures.append("backlog 含可还原明文")

all_backlog_events = parse_sse(all_backlog_path)
display_text(all_backlog_events, "完整 SSE backlog")

html = pathlib.Path(snapshot_path).read_text(encoding="utf-8")
match = re.search(r"window\.__UI_BOOT__=\[(.*?)\];window\.__UI_BUSY__=", html, re.S)
if not match:
    failures.append("snapshot 缺少 __UI_BOOT__")
else:
    try:
        boot_events = json.loads("[" + match.group(1) + "]")
    except json.JSONDecodeError:
        failures.append("snapshot __UI_BOOT__ 无法解析")
    else:
        display_text(boot_events, "snapshot")

journal_events = []
for line in pathlib.Path(journal_path).read_text(encoding="utf-8").splitlines():
    try:
        journal_events.append(json.loads(line))
    except json.JSONDecodeError:
        failures.append("journal 含无法解析的事件")
model_text = "".join(str(event.get("content", "")) for event in journal_events
                    if event.get("kind") == "model/response")
if aws in model_text or pem_body in model_text:
    failures.append("journal 的 model/response 可跨事件拼接还原明文")
tool_outputs = [event for event in journal_events
                if event.get("kind") == "tool/output" and event.get("ok") is True]
if not any("[REDACTED:" in str(event.get("text", "")) for event in tool_outputs):
    failures.append("journal 没有成功的已脱敏 tool/output，shell 可能根本没执行")

if failures:
    for failure in failures:
        print("FAIL " + failure)
    raise SystemExit(1)
print("PASS 实时 SSE、精确 backlog 与 snapshot 均无可还原明文")
print("PASS journal 的 shell tool/output 已执行且带脱敏标记")
PY
then
  fail=1
fi

if [ "$fail" -eq 0 ]; then
  echo "PASS journal / SSE / snapshot 三条路径都不带明文"
else
  echo "FAIL 网页端仍未脱敏——日志干净不代表导出的快照干净"
fi
exit "$fail"
