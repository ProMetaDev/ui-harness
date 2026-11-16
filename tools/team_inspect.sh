#!/usr/bin/env bash
set -uo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
ROOT=/tmp/team-m
NODE=$(command -v node || echo /home/fairy/.nvm/versions/node/v22.23.3/bin/node)

echo "════════ code 任务：改完的 math.js ════════"
cat "$ROOT/code/math.js"
echo
echo "--- 行为验证（旧行为 + 新行为）---"
"$NODE" --input-type=module -e '
import("file:///tmp/team-m/code/math.js").then(m => {
  const out = [];
  out.push(["add(1,2)", m.add(1,2), 3]);
  out.push(["add(1,2,3)", m.mul ? m.add(1,2,3) : null, 6]);
  out.push(["mul(2,3)", m.mul(2,3), 6]);
  let divThrew = false;
  try { m.div(1,0) } catch (e) { divThrew = true }
  out.push(["div(1,0) 抛错", divThrew, true]);
  let bad = 0;
  for (const [name, got, want] of out) {
    const ok = JSON.stringify(got) === JSON.stringify(want);
    if (!ok) bad++;
    console.log((ok ? "PASS" : "FAIL") + "  " + name + " = " + JSON.stringify(got) + " (期望 " + JSON.stringify(want) + ")");
  }
  process.exit(bad ? 1 : 0);
});
'
echo "code 验证退出码: $?"

echo
echo "════════ docs 任务：CONTRIBUTING.md 前 40 行 ════════"
head -40 "$ROOT/docs/CONTRIBUTING.md"

echo
echo "════════ analysis 任务：最终结论（从日志尾部取）════════"
tail -30 "$ROOT/analysis.log"
