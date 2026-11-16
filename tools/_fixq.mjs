import fs from 'node:fs';
import path from 'node:path';
const DIR = 'X:/dsh_ws/harness-v2/tools';
let fixed = 0;
for (const f of fs.readdirSync(DIR).filter((x) => x.endsWith('.sh'))) {
  const full = path.join(DIR, f);
  let t = fs.readFileSync(full, 'utf8');
  const before = t;
  // 行尾多出的引号：KEY="$VAR""  → KEY="$VAR"
  t = t.replace(/^(\s*(?:export\s+)?[A-Z_]+="\$[A-Z_]+")"\s*$/gm, '$1');
  if (t !== before) {
    fs.writeFileSync(full, t, 'utf8');
    fixed++;
    console.log('修复:', f);
  }
}
console.log('共修', fixed, '个');
