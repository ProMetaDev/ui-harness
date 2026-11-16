// 从 DSH 的 app.asar 里取事实：有哪些能力关键词、在哪些文件里。
import fs from 'node:fs';
const ASAR = process.argv[2] || 'D:/deepseek_harness/resources/app.asar';

function readHeader(file) {
  const fd = fs.openSync(file, 'r');
  const head = Buffer.alloc(16);
  fs.readSync(fd, head, 0, 16, 0);
  const jsonLen = head.readUInt32LE(12);
  const json = Buffer.alloc(jsonLen);
  fs.readSync(fd, json, 0, jsonLen, 16);
  fs.closeSync(fd);
  return { json: JSON.parse(json.toString('utf8')), dataOffset: 16 + jsonLen };
}

function walk(node, prefix, out) {
  for (const [name, v] of Object.entries(node.files || {})) {
    const p = prefix ? `${prefix}/${name}` : name;
    if (v.files) walk(v, p, out);
    else out.push({ path: p, size: v.size || 0, offset: v.offset });
  }
}

const { json, dataOffset } = readHeader(ASAR);
const files = [];
walk(json, '', files);
console.log('文件总数:', files.length);

const KW = ['mcp', 'subagent', 'sub-agent', 'approval', 'permission', 'sandbox', 'terminal',
  'tool', 'session', 'workflow', 'skill', 'memory', 'compact', 'resume', 'agent'];

// 按路径命中统计
const byKeyword = {};
for (const kw of KW) byKeyword[kw] = files.filter(f => f.path.toLowerCase().includes(kw));

for (const kw of KW) {
  const hits = byKeyword[kw];
  console.log(`${kw.padEnd(10)} ${String(hits.length).padStart(4)} 个路径命中`);
}

console.log('\n=== 体积最大的 15 个文件 ===');
files.sort((a, b) => b.size - a.size).slice(0, 15)
  .forEach(f => console.log((f.size / 1024).toFixed(0).padStart(6) + ' KB  ' + f.path));

console.log('\n=== 含 mcp 的路径（前 15）===');
byKeyword.mcp.slice(0, 15).forEach(f => console.log('  ' + f.path));
console.log('\n=== 含 subagent 的路径（前 15）===');
byKeyword.subagent.slice(0, 15).forEach(f => console.log('  ' + f.path));
console.log('\n=== 含 tool 的路径（前 20）===');
byKeyword.tool.slice(0, 20).forEach(f => console.log('  ' + f.path));
