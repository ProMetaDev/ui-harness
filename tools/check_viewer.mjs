// 校验导出的查看器：内嵌 JSON 必须能解析，且结构与查看器的假设一致。
import fs from 'node:fs';

const file = process.argv[2] || 'out/trace.html';
const html = fs.readFileSync(file, 'utf8');

const m = html.match(
  /<script id="ui-data" type="application\/json">([\s\S]*?)<\/script>/,
);
if (!m) {
  console.error('FAIL 找不到内嵌数据块');
  process.exit(1);
}

let data;
try {
  data = JSON.parse(m[1]);
} catch (e) {
  console.error('FAIL 内嵌 JSON 解析失败:', e.message);
  process.exit(1);
}

const events = data.events || [];
const steps = events.filter((e) => e.kind === 'prompt/snapshot');
const calls = events.filter((e) => e.kind === 'tool/request');
const decisions = {};
for (const c of calls) decisions[c.decision] = (decisions[c.decision] || 0) + 1;

console.log('ok   事件数  ', events.length);
console.log('ok    步数   ', steps.length);
console.log('ok   工具调用', calls.length, JSON.stringify(decisions));
console.log('ok   预算上限', JSON.stringify(data.limits));
console.log('ok   无外链  ', !/https?:\/\//.test(html));

const bad = steps.filter((s) => !Array.isArray(s.sections) || typeof s.tokens !== 'number');
if (bad.length) {
  console.error('FAIL 有快照缺少 sections/tokens');
  process.exit(1);
}
