#!/usr/bin/env bash
# 隐私这条链的实跑验收：落账前脱敏（模式档）＋ 按来源扣留（清单档）＋
# 两个落盘目录的权限不对称 ＋ 保留期与自动清 ＋ --no-redact 的代价。
#
# 为什么单独一个脚本：内存里的字段断言证明不了"不出盘"，而"不出盘"正是这一组
# 检查唯一关心的事。所以它跑真二进制、读真日志文件、stat 真权限位。
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"
BIN="$UI_BIN"
if [ ! -x "$BIN" ]; then
  echo "没有可执行的 $BIN —— 先构建：cargo build --release -p ui-cli"
  exit 1
fi
# 工作区放在本机文件系统，不放可能不带 unix 权限位的挂载盘：
# 0600/0700 那两条在 drvfs（Windows 盘）上永远是 777，验不出真东西。
WS=${TMPDIR:-/tmp}/ui-verify-privacy
pass=0; fail=0
ck() { case "$3" in *"$2"*) pass=$((pass+1)); echo "  ✓ $1";;
    *) fail=$((fail+1)); echo "  ✗ $1"; echo "      期望含: $2"; echo "      实得: $(head -c 260 <<<"$3")";; esac }
nock() { case "$3" in *"$2"*) fail=$((fail+1)); echo "  ✗ $1（出现了不该出现的 $2）"; echo "      $(head -c 260 <<<"$3")";;
    *) pass=$((pass+1)); echo "  ✓ $1";; esac }
yesno() { if [ "$2" -eq 0 ]; then pass=$((pass+1)); echo "  ✓ $1"; else fail=$((fail+1)); echo "  ✗ $1（退出码 $2）"; fi; }

# 三种 canary 各自只被一档挡住——这样才能看出哪一档真的在工作：
#  AKIA…/ghp_… 有凭据形状 → 模式脱敏负责
#  OPAQUE…   没有形状      → 只有"它从哪来"能救（清单档）
K1=AKIAIOSFODNN7EXAMPLE  # 测试夹具：密钥形状，非真凭据（allowlist secret）
K2=ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
PRIVATE=OPAQUE-CONFIG-88231-not-a-key-shape

rm -rf "$WS"
mkdir -p "$WS/secrets" "$WS/notes"
printf '备份用 %s 结束\n' "$K1" > "$WS/notes.txt"
printf '%s\n' "$PRIVATE" > "$WS/secrets/api.txt"
printf '# p1a\n' > "$WS/README.md"
cat > "$WS/mock.json" <<JSON
{"model":"scripted","steps":[
 {"content":"先看一眼笔记。",
  "toolCalls":[{"name":"fs_read","args":{"path":"notes.txt"}}]},
 {"content":"抄一份出去。",
  "toolCalls":[{"name":"fs_write","args":{"path":"out/copy.txt","content":"抄一份 $K2\n"}}]},
 {"content":"再取私密目录里的那份配置。",
  "toolCalls":[{"name":"shell","args":{"command":"cat secrets/api.txt"}}]},
 {"content":"三样都办完了。"}]}
JSON
J() { cat "$WS/.ui/journal.jsonl" 2>/dev/null; }

echo "═══ 1. 默认这一档：日志里没有原文，但有「抹过几处」的记号 ═══"
$BIN --workspace "$WS" run --task "把三份东西过一遍" --mock "$WS/mock.json" --yes >out/p1a-run1.log 2>&1
rc=$?; yesno "run 正常结束" $rc
nock "AKIA 不进日志"      "$K1"      "$(J)"
nock "ghp 不进日志"       "$K2"      "$(J)"
nock "私密目录的内容不进日志" "$PRIVATE" "$(J)"
ck   "输出侧留了抹除记号"  "[REDACTED:aws key]"  "$(J)"
ck   "抹了几处要入账"      '"redacted":'          "$(J)"
ck   "命中清单的那条被扣留" '"withheld":"secrets"' "$(J)"
ck   "扣留的那条正文是空的" '"text":"","withheld"'  "$(J)"
ck   "扣留留了字数与指纹"    '"chars":'              "$(grep -o '"chars":[0-9]*,"sha256":"[0-9a-f]*"' <<<"$(J)" | head -1)"
ck   "replay 说清正文没落账" "正文未落账"            "$($BIN --workspace "$WS" replay 2>&1)"
ck   "replay 说清抹了几处"    "落账前抹掉"            "$($BIN --workspace "$WS" replay 2>&1)"

echo "═══ 2. 落盘的原文：0600、在只给人看的那个目录、模型读不走 ═══"
SPILL=$(ls "$WS/.ui/tool-private/" 2>/dev/null | grep -- '-shell.txt' | head -1)
if [ -n "${SPILL:-}" ]; then pass=$((pass+1)); echo "  ✓ 扣留的原文有落盘文件（$SPILL）"; else fail=$((fail+1)); echo "  ✗ 没有落盘文件，指针会指空"; fi
ck   "指针指的就是那个文件" "$SPILL" "$(J)"
nock "模型可读的那个目录没留第二份" "$SPILL" "$(ls "$WS/.ui/tool-output/" 2>/dev/null)"
ck   "落盘文件里是原文"     "$PRIVATE" "$(cat "$WS/.ui/tool-private/$SPILL" 2>/dev/null)"
MODE=$(stat -c '%a' "$WS/.ui/tool-private/$SPILL" 2>/dev/null)
ck   "权限是 600" "600" "$MODE"
DMODE=$(stat -c '%a' "$WS/.ui/tool-private" 2>/dev/null)
ck   "目录本身是 700" "700" "$DMODE"
# 指针不是通行证。日志里那句"全文在哪"是给模型看的提示词——它要是能自己
# fs_read 回来，落账时的第二层扣留就只是把正文挪了个地方。
cat > "$WS/mock-grab.json" <<JSON
{"model":"scripted","steps":[
 {"content":"日志说全文在那个文件里，读回来。",
  "toolCalls":[{"name":"fs_read","args":{"path":".ui/tool-private/$SPILL"}}]},
 {"content":"读完了。"}]}
JSON
$BIN --workspace "$WS" run --task "把那份全文读回来" --mock "$WS/mock-grab.json" --yes >out/p1a-grab.log 2>&1
R=$($BIN --workspace "$WS" replay 2>&1)
ck   "模型去读指针：明确被告知被挡" "受保护目录" "$R"
nock "被挡就是被挡，不许报「文件不存在」误导" "文件不存在" "$R"
nock "正文没有从旁门回到上下文"     "$PRIVATE"          "$R"
D=$($BIN --workspace "$WS" doctor 2>&1)
ck   "doctor 报出落盘脚印" "落盘原文（" "$D"
ck   "doctor 说清保留策略" "保留策略"   "$D"

echo "═══ 3. 执行没被改动：脱敏管的是日志，不是数据 ═══"
ck   "笔记文件原样"     "$K1" "$(cat "$WS/notes.txt")"
ck   "模型抄的那份原样" "$K2" "$(cat "$WS/out/copy.txt" 2>/dev/null)"
ck   "私密文件原样"     "$PRIVATE" "$(cat "$WS/secrets/api.txt")"

echo "═══ 4. ui secrets：此刻能出门的两样东西都是干净的 ═══"
out=$($BIN --workspace "$WS" secrets 2>&1); rc=$?
yesno "journal 与 tool-output 里没有凭据形状" $((rc == 0 ? 0 : 1))
ck "报告说干净" "未发现疑似密钥" "$out"

echo "═══ 5. --no-redact：原文回来，代价也留档 ═══"
rm -rf "$WS/.ui" "$WS/out"
$BIN --workspace "$WS" --no-redact run --task "同上" --mock "$WS/mock.json" --yes >out/p1a-run2.log 2>&1
rc=$?; yesno "run 正常结束" $rc
ck   "日志里有 AKIA 原文" "$K1" "$(J)"
ck   "日志里有 ghp 原文"  "$K2" "$(J)"
ck   "这一轮的开始记了一条 redact-off" '"kind":"privacy/redact-off"' "$(J)"
ck   "记号说清是被哪个开关关的" "no-redact" "$(J)"
nock "按来源扣留不受这个开关影响：私密内容仍不在日志里" "$PRIVATE" "$(J)"
ck   "扣留那一档照旧写 withheld" '"withheld":"secrets"' "$(J)"
nock "关掉了就不报抹除计数" '"redacted":' "$(J)"
n=$(grep -c '"kind":"privacy/redact-off"' "$WS/.ui/journal.jsonl")
yesno "一轮只记一条" $((n == 1 ? 0 : 1))
out=$($BIN --workspace "$WS" secrets 2>&1); rc=$?
ck "ui secrets 现在指得到日志里的那一行" "journal 第" "$out"
[ $rc -ne 0 ] && { pass=$((pass+1)); echo "  ✓ 关了脱敏之后 ui secrets 用非零退出报警"; } || { fail=$((fail+1)); echo "  ✗ 应该报警"; }
out=$($BIN --workspace "$WS" --no-redact doctor 2>&1)
ck "doctor 不受这个开关影响（照样报脚印）" "落盘原文" "$out"

echo "═══ 6. 门不在 agent 身上：网页端与独立 Journal 的落账点也要过门 ═══"
# 网页端 worker 会替人补一笔「排队时被撤掉的消息」，正文是人自己打的字。
# 这一路过去直接 journal.append，实测把 AKIA… 明文写进了日志。
rm -rf "$WS/.ui"
# 这一节要的是"worker 正忙、第二条消息只能排队"，所以剧本里放一条慢命令
cat > "$WS/mock-slow.json" <<JSON
{"model":"scripted","steps":[
 {"content":"先做一件慢事。",
  "toolCalls":[{"name":"shell","args":{"command":"sleep 8 && echo 慢完了"}}]},
 {"content":"收尾。"}]}
JSON
PORT=$(ui_free_port 17933)
JAR=/tmp/ui-verify-privacy-cookie.txt
SLOG=/tmp/ui-verify-privacy-serve.log
: > "$SLOG"
$BIN serve --workspace "$WS" --port $PORT --mode strict --yes --mock "$WS/mock-slow.json" \
  > "$SLOG" 2>&1 &
SERVE_PID=$!
# 只收自己起的那个进程。以前这里 kill 掉全部 `ui serve`，
# 于是跑一次验收会顺手杀掉别人正在用的演示服务器。
trap 'kill ${SERVE_PID:-0} 2>/dev/null' EXIT
for _ in $(seq 1 150); do grep -q 'UI 已启动' "$SLOG" && break; sleep 0.1; done
TOK=$(sed -n 's/.*?t=\([0-9a-f]*\).*/\1/p' "$SLOG" | head -1)
rm -f "$JAR"; curl -s -c "$JAR" "http://127.0.0.1:$PORT/?t=$TOK" -o /dev/null
sp() { curl -s -b "$JAR" -X POST "http://127.0.0.1:$PORT$1" -H 'content-type: application/json' -d "$2"; }
sp "/api/say" '{"text":"开始"}' >/dev/null
for _ in $(seq 1 100); do
  case "$(curl -s -b "$JAR" "http://127.0.0.1:$PORT/api/state")" in
    *'"busy":true'*) break;;
  esac; sleep 0.1
done
S=$(sp "/api/say" "{\"text\":\"部署要用 $K1，先别贴进 issue\"}")
TID=$(sed -n 's/.*"taskId":"\([^"]*\)".*/\1/p' <<<"$S")
sp "/api/cancel" "{\"taskId\":\"$TID\"}" >/dev/null
for _ in $(seq 1 400); do grep -q '"kind":"turn/skipped"' "$WS/.ui/journal.jsonl" 2>/dev/null && break; sleep 0.1; done
ck   "撤掉的那条照样记了一笔"    '"kind":"turn/skipped"' "$(J)"
nock "人打的字同样没留在日志里"  "$K1"                  "$(J)"
ck   "这里也盖了抹除计数"        '"redacted":1'         "$(J)"
kill "$SERVE_PID" 2>/dev/null; sleep 0.3

# 记忆库这一头：`.ui/memory.jsonl` 就是要存原文（召回时那是内容本身，
# 抹了等于记错），所以它**故意**不走门。两道补偿：写的时候凭据形状直接被拒，
# 以及读的时候 ui secrets 必须扫得到它——不能"扫不到就等于不存在"。
out=$($BIN --workspace "$WS" remember "部署 token 是 $K2" 2>&1); rc=$?
[ $rc -ne 0 ] && { pass=$((pass+1)); echo "  ✓ 凭据形状写不进记忆库"; } \
               || { fail=$((fail+1)); echo "  ✗ 居然收下了：$out"; }
ck "拒绝时说清命中哪一类" "ghp_" "$out"
$BIN --workspace "$WS" remember "上线前先跑门禁 bash tools/ci.sh" >out/p1a-mem.log 2>&1
yesno "普通记忆照旧写得进" $?
ck "记忆库存的就是原文" "tools/ci.sh" "$(cat "$WS/.ui/memory.jsonl" 2>/dev/null)"
# 手工补一条"上一版没拦住的"记录：扫描器要能把它指出来
printf '{"kind":"memory/write","text":"顺手记一下 %s"}\n' "$K1" >> "$WS/.ui/memory.jsonl"
out=$($BIN --workspace "$WS" secrets 2>&1); rc=$?
ck "ui secrets 连记忆库一起扫" "memory 第" "$out"
[ $rc -ne 0 ] && { pass=$((pass+1)); echo "  ✓ 记忆库里的凭据用非零退出报警"; } \
               || { fail=$((fail+1)); echo "  ✗ 没报警：$out"; }

echo "═══ 7. 保留策略：两个目录一起过期，但只过期内核写的那些 ═══"
# 第 2 节验的是"原文出盘时是 0600、模型读不走"，这一节验的是"出了盘之后归谁管"。
# 两种原文都要有：.ui/tool-output/ 是模型能按需读回的截断缓存，
# .ui/tool-private/ 是按清单扣留的正文——上限按两目录合计算，只扫一个就是假的。
rm -rf "$WS/.ui" "$WS/out"
cat > "$WS/mock-both.json" <<JSON
{"model":"scripted","steps":[
 {"content":"来一段长输出。",
  "toolCalls":[{"name":"shell","args":{"command":"seq 1 4000 | sed 's/^/row /'"}}]},
 {"content":"再取私密目录里的那份配置。",
  "toolCalls":[{"name":"shell","args":{"command":"cat secrets/api.txt"}}]},
 {"content":"两件都办了。"}]}
JSON
rel() { grep -o "\"file\":\"$1[^\"]*\"" "$WS/.ui/journal.jsonl" | tail -1 | sed 's/^"file":"//; s/"$//'; }
$BIN --workspace "$WS" run --task "两种原文各来一份" --mock "$WS/mock-both.json" --tool-chars 300 --yes >out/p1a-r7.log 2>&1
yesno "run 正常结束" $?
TRIM=$(rel '.ui/tool-output/'); HELD=$(rel '.ui/tool-private/')
[ -n "$TRIM" ] && [ -f "$WS/$TRIM" ]; yesno "截断缓存落了盘（${TRIM##*/}）" $?
[ -n "$HELD" ] && [ -f "$WS/$HELD" ]; yesno "扣留原文落了另一个盘（${HELD##*/}）" $?
out=$($BIN --workspace "$WS" spill 2>&1)
ck "ui spill 把两个目录合计算" "现存 2 个文件" "$out"
ck "刚写的不该被算成会清"      "0 个、0 B 会被清掉" "$out"
ck "策略那一档要说得出天数"    "7 天 / 200 个 / 256 MiB" "$out"
ck "指得到截断缓存的原名"      "${TRIM##*/}" "$out"
ck "指得到扣留原文的原名"      "${HELD##*/}" "$out"
ck "扣留的那一份要标出来源"    "扣留原文" "$out"
ck "窗口内的标记是「留下」"    "留下" "$out"

# 人自己往这两个目录放了东西：清理的边界是"名字是不是内核的"
printf '我自己放的笔记\n' > "$WS/.ui/tool-output/keepme.txt"
out=$($BIN --workspace "$WS" spill 2>&1)
ck "非内核命名的要如实报数" "其中 1 个不是内核命名" "$out"
out=$($BIN --workspace "$WS" spill --json 2>&1)
ck "json 里也带着那份"       '"name":"keepme.txt"' "$out"
ck "json 说它不是我的"       '"mine":false' "$out"
ck "json 说得出它在哪个目录" '"from":"tool-output"' "$out"

echo "--- 过期之后：先给清单，再动手 ---"
touch -d '30 days ago' "$WS/$TRIM" "$WS/$HELD"
out=$($BIN --workspace "$WS" spill clean --dry-run 2>&1)
ck "预览点名截断缓存" "${TRIM##*/}" "$out"
ck "预览点名扣留原文" "${HELD##*/}" "$out"
ck "预览说会清 2 个"  "按这一档：2 个" "$out"
ck "预览声明没动盘"   "没有动盘" "$out"
[ -f "$WS/$TRIM" ] && [ -f "$WS/$HELD" ]; yesno "dry-run 之后两份都还在" $?
out=$($BIN --workspace "$WS" spill clean 2>&1)
ck "真清说删了几个"   "已清：删掉 2 个" "$out"
[ ! -f "$WS/$TRIM" ] && [ ! -f "$WS/$HELD" ]; yesno "过期的两份真没了" $?
[ -f "$WS/.ui/tool-output/keepme.txt" ]; yesno "人的文件一个字没动" $?

echo "--- 账上那条指针：断了要说，不能装看不见 ---"
out=$($BIN --workspace "$WS" doctor 2>&1)
ck "doctor 报出断掉的指针"   "指向已过保留期的原文" "$out"
ck "doctor 说清还能核对什么" "字数与指纹仍可核对"   "$out"
cat > "$WS/mock-read.json" <<JSON
{"model":"scripted","steps":[
 {"content":"把上次那份全文再读一眼。",
  "toolCalls":[{"name":"fs_read","args":{"path":"$TRIM"}}]},
 {"content":"读完了。"}]}
JSON
$BIN --workspace "$WS" run --task "读回来" --mock "$WS/mock-read.json" --yes >out/p1a-r7b.log 2>&1
# 这句诊断落在**日志**里（replay 给的是摘要行），所以断言打在 journal 上
ck "fs_read 说的是「已清理」不是「不存在」" "已按保留策略清理" "$(J)"
nock "而且它没被当成隐私来源扣留" '"withheld"'          "$(grep '已按保留策略清理' "$WS/.ui/journal.jsonl")"
nock "顺手确认它没有编出一段正文" "这里没落盘过"        "$(J)"
# replay 那一行不能把一次失败读成"按需展开成功"
LINE=$($BIN --workspace "$WS" replay 2>&1 | grep 'tool/output.*fs_read' | tail -1)
nock "replay 不把它误报成全量落盘" "全量落盘" "$LINE"
ck   "replay 仍报得出模型看了多少字" "模型看到" "$LINE"

echo "--- 自动清：run 边界自己动，不用人记着 ---"
$BIN --workspace "$WS" run --task "再来一次" --mock "$WS/mock-both.json" --tool-chars 300 --yes >out/p1a-r7c.log 2>&1
TRIM2=$(rel '.ui/tool-output/'); HELD2=$(rel '.ui/tool-private/')
[ -f "$WS/$TRIM2" ] && [ -f "$WS/$HELD2" ]; yesno "这一轮两个目录各落一份" $?
touch -d '30 days ago' "$WS/$TRIM2" "$WS/$HELD2"
cat > "$WS/mock-idle.json" <<JSON
{"model":"scripted","steps":[{"content":"这轮什么都不做。"}]}
JSON
$BIN --workspace "$WS" run --task "空转一轮" --mock "$WS/mock-idle.json" --yes >out/p1a-r7d.log 2>&1
ck "清理这件事本身留了账" '"kind":"privacy/spill-pruned"' "$(J)"
ck "账上把两个目录合起来记" '"removed":2' "$(J)"
[ ! -f "$WS/$TRIM2" ] && [ ! -f "$WS/$HELD2" ]; yesno "没跑 ui spill 也清掉了两个目录" $?

echo "--- 两个档位：全留 与 一个不留 ---"
$BIN --workspace "$WS" run --task "第三种" --mock "$WS/mock-both.json" --tool-chars 300 --yes >out/p1a-r7e.log 2>&1
TRIM3=$(rel '.ui/tool-output/'); HELD3=$(rel '.ui/tool-private/')
touch -d '30 days ago' "$WS/$TRIM3" "$WS/$HELD3"
out=$(UI_SPILL_KEEP=all $BIN --workspace "$WS" spill clean 2>&1)
ck "全留档要自己说清楚" "现在是「全留」" "$out"
[ -f "$WS/$TRIM3" ] && [ -f "$WS/$HELD3" ]; yesno "全留=过期也不删" $?
out=$($BIN --workspace "$WS" spill clean --all 2>&1); rc=$?
[ $rc -ne 0 ] && { pass=$((pass+1)); echo "  ✓ --all 不带 --yes 就拒绝动手"; } \
               || { fail=$((fail+1)); echo "  ✗ 它直接删了：$out"; }
ck "拒绝时说了代价" "会清掉**全部**落盘原文" "$out"
out=$($BIN --workspace "$WS" spill clean --all --yes 2>&1)
ck "--all --yes 两个目录一起清" "已清：删掉 2 个" "$out"
[ ! -f "$WS/$TRIM3" ] && [ ! -f "$WS/$HELD3" ]; yesno "过期没过期都不留" $?
[ -f "$WS/.ui/tool-output/keepme.txt" ]; yesno "--all 也不碰外来的文件" $?
out=$(UI_SPILL_MAX_DAYS=0 UI_SPILL_MAX_BYTES=1k $BIN --workspace "$WS" spill 2>&1)
ck "环境变量这一路也认" "0 天 / 200 个 / 1 MiB" "$out"

echo; echo "通过 $pass / 失败 $fail"
# 结论进门禁汇总：非零即 FAIL（tools/debug.sh 会 tail 输出）
[ $fail -eq 0 ]
