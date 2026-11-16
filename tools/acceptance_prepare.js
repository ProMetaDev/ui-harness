// 验收准备：复制项目 → 注入一个真实的 bug（不动测试）。
// 注入点：policy.rs 里 glob 的 `?` 分支少消费一个字符，只有 src/?ain.rs 这条断言会挂。
const fs = require('fs');
const path = require('path');

const SRC = '/mnt/x/dsh_ws/harness-v2';
const DST = process.argv[2] || '/tmp/acceptance/repo';

function copy(src, dst) {
  fs.mkdirSync(dst, { recursive: true });
  for (const entry of fs.readdirSync(src, { withFileTypes: true })) {
    const skip = ['.ui', 'out', 'target', 'secrets', '.git', 'node_modules', 'demo'];
    if (skip.includes(entry.name)) continue;
    const from = path.join(src, entry.name);
    const to = path.join(dst, entry.name);
    if (entry.isDirectory()) copy(from, to);
    else if (entry.isFile()) fs.copyFileSync(from, to);
  }
}

fs.rmSync(DST, { recursive: true, force: true });
copy(SRC, DST);

const policy = path.join(DST, 'ui-core/src/policy.rs');
let text = fs.readFileSync(policy, 'utf8');
const good = "'?' => !t.is_empty() && matches_at(&p[1..], &t[1..]),";
const bad = "'?' => matches_at(&p[1..], t),";
if (!text.includes(good)) {
  console.error('注入点没找到，测试文件可能已变');
  process.exit(1);
}
fs.writeFileSync(policy, text.replace(good, bad), 'utf8');
console.log(`注入完成 → ${DST}`);
