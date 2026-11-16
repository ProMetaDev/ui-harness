// 第二例：把 fs_write 的"原子写"去掉（临时文件 + rename）。
// 这个改动不会让任何测试变红——只能靠读代码、想崩溃语义来发现。
const fs = require('fs');
const path = require('path');

const SRC = '/mnt/x/dsh_ws/harness-v2';
const DST = process.argv[2] || '/tmp/acceptance/repo2';

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

const tools = path.join(DST, 'ui-core/src/tools.rs');
const text = fs.readFileSync(tools, 'utf8');
const start = text.indexOf('    let tmp = p.with_extension(');
const end = text.indexOf('    ToolResult::ok(format!("写入 {} 字节到 {}", content.len(), p.display()))');
if (start < 0 || end <= start) {
  console.error('注入点没找到');
  process.exit(1);
}
const replacement = `    if let Err(e) = fs::write(&p, &content) {
        return ToolResult::err(format!("写入失败: {}", e));
    }
`;
fs.writeFileSync(tools, text.slice(0, start) + replacement + text.slice(end), 'utf8');
console.log(`注入完成（去掉了原子写）→ ${DST}`);
