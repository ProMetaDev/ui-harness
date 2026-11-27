#!/usr/bin/env bash
# 自测真实多仓库评测框架：只跑合成 fixture + dry-run/index。
#
# 不调用任何真实 provider，不产生真实评测结果。没有 ui 二进制时 SKIP，
# 因为 index 必须由真实 ui 二进制完成——不能拿假数据冒充。
#
#   bash tools/verify_bench_repos.sh
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
cd "$UI_ROOT"

PASS=0
FAIL=0
ok() {
  PASS=$((PASS + 1))
  printf '  PASS %s\n' "$1"
}
bad() {
  FAIL=$((FAIL + 1))
  printf '  FAIL %s\n' "$1"
  [ -n "${2:-}" ] && printf '       %s\n' "$2"
}

if ! command -v python3 >/dev/null 2>&1; then
  echo "SKIP bench_repos：没有 python3"
  exit 0
fi
if [ ! -x "${UI_BIN:-}" ]; then
  echo "SKIP bench_repos：没有 ui 二进制（$UI_BIN）；index 不能伪造，先构建"
  exit 0
fi
for f in tools/bench_repos.py tools/make_bench_fixtures.sh bench/tasks.example.json bench/repos.example.json; do
  if [ ! -f "$f" ]; then
    echo "FAIL bench_repos：缺少 $f"
    exit 1
  fi
done

PY=python3
TMP=$(mktemp -d "${TMPDIR:-/tmp}/ui-bench-repos.XXXXXX") || exit 1
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

echo "=== 1. 合成 fixture（不是真实仓库）==="
if bash tools/make_bench_fixtures.sh "$TMP/fixtures" >"$TMP/fixtures.log" 2>&1; then
  ok "make_bench_fixtures.sh 退出 0"
else
  bad "make_bench_fixtures.sh 失败" "$(tail -5 "$TMP/fixtures.log" 2>/dev/null)"
fi
fixture_count=$(find "$TMP/fixtures" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')
if [ "$fixture_count" -ge 12 ]; then
  ok "生成了 $fixture_count 个合成仓库（要求 >= 12）"
else
  bad "合成仓库只有 $fixture_count 个（要求 >= 12）"
fi
small=0
while IFS= read -r d; do
  [ -d "$d" ] || continue
  lines=$(find "$d" -type f -exec cat {} + 2>/dev/null | wc -l | tr -d ' ')
  [ "$lines" -lt 1000 ] && small=$((small + 1))
  if ! grep -qF "合成 fixture，不是真实仓库" "$d/SYNTHETIC_FIXTURE.md" 2>/dev/null; then
    bad "fixture $d 缺少“不是真实仓库”标记"
  fi
done < <(find "$TMP/fixtures" -mindepth 1 -maxdepth 1 -type d | sort)
if [ "$small" -eq 0 ]; then
  ok "每个 fixture 都 >= 1000 行"
else
  bad "$small 个 fixture 少于 1000 行"
fi

$PY - "$TMP/fixtures" "$TMP/repos.json" <<'PY'
import json, os, sys
root, out = sys.argv[1], sys.argv[2]
rows = []
for name in sorted(os.listdir(root)):
    path = os.path.join(root, name)
    if not os.path.isdir(path):
        continue
    rows.append({"name": name, "path": path, "commit": "", "language": name.split("-", 1)[-1]})
json.dump(rows, open(out, "w", encoding="utf-8"), ensure_ascii=False, indent=2)
print("repos.json", len(rows))
PY

echo "=== 2. dry-run/index：报告结构 ==="
"$PY" tools/bench_repos.py \
  --repos "$TMP/repos.json" --tasks bench/tasks.example.json \
  --ui "$UI_BIN" --out "$TMP/dry" --timeout 120 \
  >"$TMP/dry.stdout" 2>"$TMP/dry.stderr"
dry_rc=$?
if [ "$dry_rc" -eq 0 ]; then
  ok "dry-run 退出 0"
else
  bad "dry-run 退出 $dry_rc" "$(tail -8 "$TMP/dry.stderr" 2>/dev/null)"
fi
if [ -f "$TMP/dry/report.json" ] && [ -f "$TMP/dry/report.md" ]; then
  ok "生成了 report.json 与 report.md"
else
  bad "缺少 report.json / report.md"
fi
if $PY - "$TMP/dry/report.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1], encoding="utf-8"))
assert r["schema"] == 1, r.get("schema")
assert r["mode"] == "dry-run", r.get("mode")
assert r.get("dryRunReason"), "dry-run 必须写明原因"
assert len(r["repos"]) == 12, len(r["repos"])
for repo in r["repos"]:
    assert repo["status"] == "ok", (repo["name"], repo["status"], repo.get("statusReason"))
    idx = repo["index"]
    assert idx["rc"] == 0, (repo["name"], idx["rc"])
    assert idx["files"] >= 8, (repo["name"], idx.get("files"))
    assert idx["symbols"] > 0, (repo["name"], idx.get("symbols"))
    assert idx["indexedLines"] >= 1000, (repo["name"], idx.get("indexedLines"))
    assert len(repo["tasks"]) == 12, (repo["name"], len(repo["tasks"]))
    for task in repo["tasks"]:
        assert task["status"] == "dry-run", (repo["name"], task["id"], task["status"])
        assert task["run"]["skipped"] is True
        assert task["verify"]["skipped"] is True
        assert task["expect"]["passed"] is None
        assert task["changedFiles"] == []
s = r["summary"]
assert s["repos"]["ok"] == 12 and s["repos"]["invalid"] == 0, s
assert s["tasks"]["total"] == 144 and s["tasks"]["dryRun"] == 144, s
assert s["tasks"]["success"] == 0 and s["tasks"]["failed"] == 0, s
assert s["index"]["ok"] == 12, s["index"]
PY
then
  ok "dry-run 报告结构与 12×12 任务状态正确"
else
  bad "dry-run 报告结构断言失败"
fi
if grep -qF "没有执行模型" "$TMP/dry/report.md" 2>/dev/null; then
  ok "report.md 明确写了 dry-run 未执行模型"
else
  bad "report.md 没有 dry-run 警示"
fi
if grep -qF "success=0" "$TMP/dry/report.md" 2>/dev/null; then
  ok "report.md 把 success=0 当事实写清"
else
  bad "report.md 没有写明 success=0"
fi

echo "=== 3. 占位符 REPLACE_ME：SKIP 且不产生结果 ==="
"$PY" tools/bench_repos.py \
  --repos bench/repos.example.json --tasks bench/tasks.example.json \
  --ui "$UI_BIN" --out "$TMP/placeholder" --timeout 60 \
  >"$TMP/placeholder.stdout" 2>"$TMP/placeholder.stderr"
ph_rc=$?
if [ "$ph_rc" -eq 0 ]; then
  ok "占位符清单退出 0（SKIP 不是错误）"
else
  bad "占位符清单退出 $ph_rc"
fi
if $PY - "$TMP/placeholder/report.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1], encoding="utf-8"))
assert r["mode"] == "dry-run"
assert len(r["repos"]) == 10, len(r["repos"])
for repo in r["repos"]:
    assert repo["status"] == "skipped", (repo["name"], repo["status"])
    assert "REPLACE_ME" in repo["path"], repo["path"]
    assert repo["index"] is None
    assert len(repo["tasks"]) == 12
    assert all(t["status"] == "skipped" for t in repo["tasks"])
s = r["summary"]
assert s["repos"]["skipped"] == 10 and s["repos"]["ok"] == 0, s
assert s["tasks"]["skipped"] == 120 and s["tasks"]["success"] == 0, s
PY
then
  ok "10 个占位仓库全部 SKIP，未索引、未执行任务"
else
  bad "占位符 SKIP 断言失败"
fi
if grep -qF "SKIP repo=" "$TMP/placeholder.stderr" 2>/dev/null \
  && grep -qF "REPLACE_ME" "$TMP/placeholder.stderr" 2>/dev/null; then
  ok "stderr 明确提示占位符 SKIP 与 REPLACE_ME"
else
  bad "stderr 没有占位符 SKIP 提示"
fi

echo "=== 4. provider 命令不得写入报告 ==="
"$PY" tools/bench_repos.py \
  --repos "$TMP/repos.json" --tasks bench/tasks.example.json \
  --ui "$UI_BIN" --out "$TMP/leak" --timeout 60 \
  --provider-cmd "SECRET_PROVIDER_CMD_DO_NOT_LEAK" --dry-run \
  >"$TMP/leak.stdout" 2>"$TMP/leak.stderr"
leak_rc=$?
if [ "$leak_rc" -eq 0 ]; then
  ok "--dry-run + --provider-cmd 退出 0"
else
  bad "--dry-run + --provider-cmd 退出 $leak_rc"
fi
if grep -RqF "SECRET_PROVIDER_CMD_DO_NOT_LEAK" "$TMP/leak" 2>/dev/null; then
  bad "provider 命令原文泄漏进报告"
else
  ok "provider 命令原文没有进报告"
fi
if $PY - "$TMP/leak/report.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1], encoding="utf-8"))
assert r["mode"] == "dry-run"
assert r["model"]["kind"] == "none", r["model"]
PY
then
  ok "dry-run 下 model.kind=none"
else
  bad "dry-run 下 model 标记不正确"
fi

echo "=== 5. 越界/空目录/缺失路径：拒绝但不终止其他仓库 ==="
bash tools/make_bench_fixtures.sh "$TMP/fixtures2" >/dev/null 2>&1
mkdir -p "$TMP/bad/empty" "$TMP/outside"
printf 'outside\n' >"$TMP/outside/README.md"
$PY - "$TMP" "$TMP/fixtures2" <<'PY'
import json, sys
tmp, fixtures = sys.argv[1], sys.argv[2]
rows = [
    {"name": "valid", "path": fixtures + "/synth-rust", "language": "rust"},
    {"name": "escape", "path": "../outside"},
    {"name": "empty", "path": "empty"},
    {"name": "missing", "path": "does-not-exist"},
]
json.dump(rows, open(tmp + "/bad/repos.json", "w", encoding="utf-8"), ensure_ascii=False, indent=2)
PY
"$PY" tools/bench_repos.py \
  --repos "$TMP/bad/repos.json" --tasks bench/tasks.example.json \
  --ui "$UI_BIN" --out "$TMP/badout" --timeout 120 \
  >"$TMP/bad.stdout" 2>"$TMP/bad.stderr"
bad_rc=$?
if [ "$bad_rc" -eq 2 ]; then
  ok "无效仓库导致退出 2（而不是 0 或崩溃）"
else
  bad "无效仓库退出 $bad_rc（期望 2）"
fi
if $PY - "$TMP/badout/report.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1], encoding="utf-8"))
by = {x["name"]: x for x in r["repos"]}
assert by["valid"]["status"] == "ok", by["valid"]
assert by["valid"]["index"]["rc"] == 0, by["valid"]
assert by["escape"]["status"] == "invalid"
assert ".." in by["escape"]["statusReason"] and "越界" in by["escape"]["statusReason"], by["escape"]
assert by["empty"]["status"] == "invalid" and "为空" in by["empty"]["statusReason"], by["empty"]
assert by["missing"]["status"] == "invalid" and "不存在" in by["missing"]["statusReason"], by["missing"]
s = r["summary"]
assert s["repos"]["ok"] == 1 and s["repos"]["invalid"] == 3, s
assert s["tasks"]["dryRun"] == 12 and s["tasks"]["skipped"] == 36, s
PY
then
  ok "越界/空目录/缺失被逐条记录，合法仓库仍被索引"
else
  bad "失败隔离/越界拒绝断言失败"
fi
if [ ! -e "$TMP/outside/.ui" ]; then
  ok "越界目录没有被索引（没有 .ui）"
else
  bad "越界目录被索引了"
fi

echo "=== 6. --provider-cmd 只经环境变量传递（假 provider，不联网）==="
mkdir -p "$TMP/provider/repo"
printf '# provider probe\n' >"$TMP/provider/repo/README.md"
cat >"$TMP/provider/tasks.json" <<'JSON'
[{"id":"t1","prompt":"hello","expect":{"answer_min_chars":1}}]
JSON
$PY - "$TMP" <<'PY'
import json, sys
tmp = sys.argv[1]
rows = [{"name": "provider-probe", "path": tmp + "/provider/repo"}]
json.dump(rows, open(tmp + "/provider/repos.json", "w", encoding="utf-8"), ensure_ascii=False)
PY
"$PY" tools/bench_repos.py \
  --repos "$TMP/provider/repos.json" --tasks "$TMP/provider/tasks.json" \
  --ui "$UI_BIN" --out "$TMP/provider/out" --timeout 60 \
  --provider-cmd "printf 'PROVIDER_REACHED\n' >&2; exit 7 # CMD_LEAK_CANARY" \
  >"$TMP/provider.stdout" 2>"$TMP/provider.stderr"
prov_rc=$?
if [ "$prov_rc" -eq 0 ]; then
  ok "假 provider 失败被记录为任务失败，进程仍退出 0"
else
  bad "假 provider 失败导致进程退出 $prov_rc（应隔离为任务失败）"
fi
if $PY - "$TMP/provider/out/report.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1], encoding="utf-8"))
assert r["mode"] == "real", r["mode"]
assert r["model"]["kind"] == "external", r["model"]
t = r["repos"][0]["tasks"][0]
assert t["status"] == "failed", t["status"]
run = t["run"]
assert run["rc"] not in (0, None), run["rc"]
out = (run.get("stdoutTail") or "") + (run.get("stderrTail") or "")
assert "PROVIDER_REACHED" in out, "provider-cmd 没有通过 UI_MODEL_CMD 传给子进程"
assert r["summary"]["tasks"]["failed"] == 1, r["summary"]
PY
then
  ok "--provider-cmd 经 UI_MODEL_CMD 生效，失败被隔离"
else
  bad "--provider-cmd 传递/失败隔离断言失败"
fi
if grep -RqF "CMD_LEAK_CANARY" "$TMP/provider/out" 2>/dev/null; then
  bad "provider 命令原文泄漏进报告（CMD_LEAK_CANARY）"
else
  ok "provider 命令原文没有进报告（只有它的输出）"
fi

if "$PY" tools/test_bench_expect.py >"$TMP/expect.log" 2>&1; then
  ok "expect.no_extra_changes 会把额外文件判失败"
else
  bad "expect.no_extra_changes 自测失败" "$(tail -5 "$TMP/expect.log" 2>/dev/null)"
fi
if "$PY" tools/test_bench_retry.py >"$TMP/retry.log" 2>&1; then
  ok "retry 只对 model_error 生效并累计 tokens"
else
  bad "retry 自测失败" "$(tail -5 "$TMP/retry.log" 2>/dev/null)"
fi

echo
echo "通过 $PASS / 失败 $FAIL"
if [ "$FAIL" -eq 0 ]; then
  echo "PASS bench_repos：合成 fixture 的 dry-run/index/越界/占位符/provider 隔离自测通过（不是真实评测结果）"
  exit 0
fi
echo "FAIL bench_repos：自测未通过"
exit 1
