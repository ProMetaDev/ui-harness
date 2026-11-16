// 离线校验 chat.html 的流式渲染逻辑（不需要浏览器）：
// 用一个最小 DOM 执行页面脚本，喂 __UI_BOOT__ 事件，检查气泡内容。
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const html = fs.readFileSync(path.join(HERE, '..', 'ui-cli', 'assets', 'chat.html'), 'utf8');
const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)];
if (!scripts.length) {
  console.error('FAIL 找不到页面脚本');
  process.exit(1);
}
const code = scripts[scripts.length - 1][1];

class El {
  constructor(tag = 'div') {
    this.tagName = tag;
    this.children = [];
    this.parentElement = null;
    this.className = '';
    this.style = {};
    this._text = '';
    this._html = '';
    this.classList = {
      add: (c) => { this.className = (this.className ? this.className + ' ' : '') + c; },
    };
  }
  append(...nodes) { for (const n of nodes) this.appendChild(n); }
  appendChild(n) { n.parentElement = this; this.children.push(n); return n; }
  remove() {
    if (!this.parentElement) return;
    const i = this.parentElement.children.indexOf(this);
    if (i >= 0) this.parentElement.children.splice(i, 1);
    this.parentElement = null;
  }
  querySelectorAll() { return []; }
  addEventListener() {}
  set textContent(v) { this._text = String(v); this.children = []; }
  get textContent() { return this._text; }
  set innerHTML(v) { this._html = String(v); this._text = String(v); }
  get innerHTML() { return this._html; }
}

function run(events) {
  const ids = {};
  for (const id of ['log', 'approvals', 'input', 'dot', 'state', 'tok', 'calls', 'model', 'modelStat', 'hint', 'send']) {
    ids[id] = new El(id);
  }
  const main = new El('main');
  const document = {
    getElementById: (id) => ids[id],
    createElement: (tag) => new El(tag),
    querySelector: (sel) => (sel === 'main' ? main : null),
    querySelectorAll: () => [],
  };
  const window = { __UI_BOOT__: events };
  vm.runInNewContext(code, {
    window,
    document,
    location: { search: '' },
    setTimeout,
    console,
    fetch: () => Promise.resolve({ json: () => Promise.resolve({ busy: false }) }),
  });
  return ids.log.children
    .filter((row) => row.className.includes('assistant'))
    .map((row) => (row.children[1] ? row.children[1].textContent : ''));
}

let fail = 0;
function check(name, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}  got=${JSON.stringify(got)} want=${JSON.stringify(want)}`);
  if (!ok) fail = 1;
}

// 1) 增量跨多条事件：最终 assistant 用完整文本替换，不能重复
check(
  'delta 累积后由 assistant 整段替换',
  run([
    { type: 'user', text: 'q' },
    { type: 'delta', text: '你' },
    { type: 'delta', text: '好' },
    { type: 'assistant', text: '你好' },
    { type: 'turn', reason: 'done', steps: 0, calls: 0 },
  ]),
  ['你好'],
);

// 2) 一轮里多条模型消息：每条消息结束于一个 assistant 事件，不能拼成一条
check(
  'tool 之后的下一条模型消息自成一个气泡',
  run([
    { type: 'delta', text: 'A' },
    { type: 'assistant', text: 'A' },
    { type: 'tool', name: 'fs_read', ok: true, detail: 'x' },
    { type: 'delta', text: 'B' },
    { type: 'assistant', text: 'B' },
    { type: 'turn', reason: 'done', steps: 1, calls: 1 },
  ]),
  ['A', 'B'],
);

// 3) 只调工具、content 为空的回复：裸协议增量不能留在页面上
check(
  'tool 边界收掉只有增量的临时气泡',
  run([
    { type: 'delta', text: '{"content":"' },
    { type: 'tool', name: 'fs_read', ok: true, detail: 'x' },
    { type: 'delta', text: 'C' },
    { type: 'assistant', text: 'C' },
    { type: 'turn', reason: 'done', steps: 1, calls: 1 },
  ]),
  ['C'],
);

process.exit(fail);
