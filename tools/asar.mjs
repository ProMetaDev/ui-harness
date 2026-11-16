// 零依赖 asar 读取：只读头部，按需从偏移读单个文件，不把 200MB 全塞进内存。
// 用法：
//   node tools/asar.mjs list  <app.asar> [子串过滤]
//   node tools/asar.mjs cat   <app.asar> <内部路径>
import fs from 'node:fs';

const [, , cmd, asarPath, arg] = process.argv;
if (!cmd || !asarPath) {
  console.error('用法: asar.mjs list <app.asar> [filter] | cat <app.asar> <path>');
  process.exit(2);
}

const fd = fs.openSync(asarPath, 'r');
const head = Buffer.alloc(16);
fs.readSync(fd, head, 0, 16, 0);
// 实测布局：u32@0=4，u32@4=header 总长，u32@8=总长-4，u32@12=JSON 长度，JSON 从 16 开始
const jsonSize = head.readUInt32LE(12);
const jsonBuf = Buffer.alloc(jsonSize);
fs.readSync(fd, jsonBuf, 0, jsonSize, 16);
const header = JSON.parse(jsonBuf.toString('utf8'));
const dataStart = 16 + jsonSize;

// asar 的 header 里文件项是 { files: {...} } 或 { size, offset }
function walk(node, prefix, out) {
  if (node.files) {
    for (const [name, child] of Object.entries(node.files)) {
      walk(child, prefix ? `${prefix}/${name}` : name, out);
    }
  } else if (typeof node.size === 'number') {
    out.push({ path: prefix, size: node.size, offset: node.offset });
  }
}

const files = [];
walk(header, '', files);

if (cmd === 'list') {
  const filter = arg || '';
  const matched = files.filter((f) => !filter || f.path.includes(filter));
  for (const f of matched.slice(0, 400)) {
    console.log(`${String(f.size).padStart(9)}  ${f.path}`);
  }
  console.log(`\n共 ${matched.length} 个（显示前 400）`);
} else if (cmd === 'cat') {
  const f = files.find((x) => x.path === arg);
  if (!f) {
    console.error('找不到:', arg);
    process.exit(3);
  }
  const buf = Buffer.alloc(f.size);
  fs.readSync(fd, buf, 0, f.size, dataStart + Number(f.offset));
  process.stdout.write(buf);
} else {
  console.error('未知命令:', cmd);
  process.exit(2);
}
fs.closeSync(fd);
