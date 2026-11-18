// 离线校验 chat.html 的聊天流逻辑（不需要浏览器、网络或第三方依赖）。
// 执行第一个 script IIFE，以假 EventSource 喂事件；第二个 IIFE 只抽取 UU 状态组件测试。
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const html = fs.readFileSync(path.join(HERE, '..', 'ui-cli', 'assets', 'chat.html'), 'utf8');
const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)];
if (!scripts.length) throw new Error('找不到页面脚本');
const chatCode = scripts[0][1];
const shellCode = scripts[1] ? scripts[1][1] : '';

class El {
  constructor(tag = 'div') {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.parentElement = null;
    this.className = '';
    this.style = {};
    this.dataset = {};
    this.attributes = {};
    this.listeners = {};
    this.value = '';
    this.disabled = false;
    this.hidden = false;
    this.scrollHeight = 0;
    this.scrollTop = 0;
    this.clientHeight = 0;
    this.offsetWidth = 0;
    this._text = '';
    this._html = '';
    this.classList = {
      add: (...names) => {
        const set = new Set(this.className.split(/\s+/).filter(Boolean));
        for (const name of names) set.add(name);
        this.className = [...set].join(' ');
      },
      remove: (...names) => {
        const drop = new Set(names);
        this.className = this.className.split(/\s+/).filter((name) => name && !drop.has(name)).join(' ');
      },
      contains: (name) => this.className.split(/\s+/).includes(name),
      toggle: (name, force) => {
        const on = force === undefined ? !this.classList.contains(name) : !!force;
        if (on) this.classList.add(name); else this.classList.remove(name);
        return on;
      },
    };
  }
  append(...nodes) { for (const node of nodes) this.appendChild(node); }
  appendChild(node) {
    if (node.parentElement) node.remove();
    node.parentElement = this;
    this.children.push(node);
    return node;
  }
  remove() {
    if (!this.parentElement) return;
    const siblings = this.parentElement.children;
    const index = siblings.indexOf(this);
    if (index >= 0) siblings.splice(index, 1);
    this.parentElement = null;
  }
  matches(selector) {
    if (selector.startsWith('.')) return this.classList.contains(selector.slice(1));
    return this.tagName === selector.toUpperCase();
  }
  querySelectorAll(selector) {
    const found = [];
    for (const child of this.children) {
      if (child.matches(selector)) found.push(child);
      found.push(...child.querySelectorAll(selector));
    }
    return found;
  }
  addEventListener(type, listener) {
    (this.listeners[type] || (this.listeners[type] = [])).push(listener);
  }
  dispatchEvent(event) {
    for (const listener of this.listeners[event.type] || []) listener.call(this, event);
    return true;
  }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  getAttribute(name) { return Object.hasOwn(this.attributes, name) ? this.attributes[name] : null; }
  focus() { this.focused = true; }
  click() { if (this.onclick) return this.onclick({ preventDefault() {}, stopPropagation() {} }); }
  set textContent(value) {
    for (const child of this.children) child.parentElement = null;
    this.children = [];
    this._text = String(value);
    this._html = '';
  }
  get textContent() { return this._text; }
  set innerHTML(value) {
    for (const child of this.children) child.parentElement = null;
    this.children = [];
    this._html = String(value);
    this._text = '';
  }
  get innerHTML() { return this._html; }
}

class FakeEvent {
  constructor(type) { this.type = type; }
}

class FixedDate extends Date {
  constructor(...args) { super(...(args.length ? args : [1700000000000])); }
  static now() { return 1700000000000; }
}

function createApp(fetchOverride) {
  const ids = {};
  // 从真实页面 id 建最小 DOM；脚本引用一个页面不存在的 id 时应当像浏览器一样报错。
  for (const match of html.matchAll(/\bid="([^"]+)"/g)) ids[match[1]] = new El('div');
  const main = new El('main');
  const documentElement = new El('html');
  const body = new El('body');
  const document = {
    documentElement,
    body,
    getElementById: (id) => ids[id] || null,
    createElement: (tag) => new El(tag),
    querySelector: (selector) => {
      if (selector === 'main' || selector === 'main > .scroll') return main;
      if (selector === '.approval') return ids.approvals.querySelectorAll('.approval')[0] || null;
      return null;
    },
    querySelectorAll: () => [],
  };

  let nextFrame = 1;
  const frames = new Map();
  function requestAnimationFrame(callback) {
    const id = nextFrame++;
    frames.set(id, callback);
    return id;
  }
  function flushFrames() {
    // 回调可以再排一帧；限制轮数以防测试替身自身形成死循环。
    for (let pass = 0; frames.size && pass < 100; pass++) {
      const pending = [...frames.values()];
      frames.clear();
      for (const callback of pending) callback(1700000000000);
    }
    if (frames.size) throw new Error('requestAnimationFrame 队列未收敛');
  }

  let nextTimer = 1;
  const timeouts = new Map();
  const intervals = new Map();
  const setTimeoutFake = (callback) => {
    const id = nextTimer++;
    timeouts.set(id, callback);
    return id;
  };
  const clearTimeoutFake = (id) => timeouts.delete(id);
  const setIntervalFake = (callback) => {
    const id = nextTimer++;
    intervals.set(id, callback);
    return id;
  };
  const clearIntervalFake = (id) => intervals.delete(id);

  const pendingFetch = () => new Promise(() => {});
  const fetchFake = fetchOverride || ((url) => {
    if (url === '/api/say') {
      return Promise.resolve({ ok: true, json: () => Promise.resolve({ ok: true }) });
    }
    // ready/state/sessions 在测试期间保持待定，避免异步启动回调覆盖正在检查的状态。
    return pendingFetch();
  });

  let eventSource = null;
  class FakeEventSource {
    constructor(url) {
      this.url = url;
      this.closed = false;
      eventSource = this;
    }
    close() { this.closed = true; }
  }

  const storage = new Map();
  const localStorage = {
    getItem: (key) => storage.has(key) ? storage.get(key) : null,
    setItem: (key, value) => storage.set(key, String(value)),
  };
  const moodCalls = [];
  const window = {
    addEventListener() {},
    removeEventListener() {},
    prompt: () => '',
    __uiMood: (mood) => moodCalls.push(mood),
  };
  const location = { search: '' };
  window.location = location;

  vm.runInNewContext(chatCode, {
    window,
    document,
    location,
    localStorage,
    navigator: {},
    URLSearchParams,
    Event: FakeEvent,
    EventSource: FakeEventSource,
    Date: FixedDate,
    setTimeout: setTimeoutFake,
    clearTimeout: clearTimeoutFake,
    setInterval: setIntervalFake,
    clearInterval: clearIntervalFake,
    requestAnimationFrame,
    cancelAnimationFrame: (id) => frames.delete(id),
    fetch: fetchFake,
    console,
  });

  return {
    ids,
    moodCalls,
    emit(event) {
      if (!eventSource || !eventSource.onmessage) throw new Error('聊天脚本没有建立 EventSource');
      eventSource.onmessage({ data: JSON.stringify(event) });
      flushFrames();
    },
  };
}

function run(events) {
  const app = createApp();
  for (const event of events) app.emit(event);
  return app;
}

function rows(app, kind) {
  return app.ids.log.children.filter((row) => row.classList.contains(kind));
}

function assistantTexts(app) {
  return rows(app, 'assistant').map((row) => row.children[1] ? row.children[1].textContent : '');
}

function checkUU() {
  const match = shellCode.match(/  \/\/ 看板娘 UU[\s\S]*?  window\.__uiMood = setMood;/);
  if (!match) throw new Error('找不到外壳中的 UU 状态组件');
  const uuA = new El('svg');
  const uuB = new El('svg');
  uuA.className = uuB.className = 'uu';
  const dot = new El('span');
  dot.className = 'dot busy';
  dot.dataset.mood = 'server-owned';
  const button = new El('button');
  const selectors = [];
  const document = {
    querySelectorAll(selector) {
      selectors.push(selector);
      if (selector === '.uu') return [uuA, uuB];
      if (selector === '.dot') return [dot];
      return [];
    },
    querySelector: () => null,
    getElementById: (id) => id === 'uuButton' ? button : null,
  };
  const window = {};
  vm.runInNewContext(match[0], {
    window,
    document,
    setTimeout: () => 1,
    clearTimeout: () => {},
  });
  window.__uiMood('thinking');
  return {
    uuMoods: [uuA.dataset.mood, uuB.dataset.mood],
    dotClass: dot.className,
    dotMood: dot.dataset.mood,
    selectors,
  };
}

let fail = 0;
function check(name, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}  got=${JSON.stringify(got)} want=${JSON.stringify(want)}`);
  if (!ok) fail = 1;
}

async function test(name, want, getGot) {
  try {
    check(name, await getGot(), want);
  } catch (error) {
    fail = 1;
    console.error(`FAIL  ${name}  执行异常：${error && error.stack ? error.stack : error}`);
  }
}

// 1) 增量跨多条事件：最终 assistant 用完整文本替换，不能重复
await test(
  'delta 累积后由 assistant 整段替换',
  ['你好'],
  () => assistantTexts(run([
    { type: 'user', text: 'q' },
    { type: 'delta', text: '你' },
    { type: 'delta', text: '好' },
    { type: 'assistant', text: '你好' },
    { type: 'turn', reason: 'done', steps: 0, calls: 0 },
  ])),
);

// 2) 一轮里多条模型消息：每条消息结束于一个 assistant 事件，不能拼成一条
await test(
  'tool 之后的下一条模型消息自成一个气泡',
  ['A', 'B'],
  () => assistantTexts(run([
    { type: 'delta', text: 'A' },
    { type: 'assistant', text: 'A' },
    { type: 'tool', name: 'fs_read', ok: true, detail: 'x' },
    { type: 'delta', text: 'B' },
    { type: 'assistant', text: 'B' },
    { type: 'turn', reason: 'done', steps: 1, calls: 1 },
  ])),
);

// 3) 只调工具、content 为空的回复：裸协议增量不能留在页面上
await test(
  'tool 边界收掉只有增量的临时气泡',
  ['C'],
  () => assistantTexts(run([
    { type: 'delta', text: '{"content":"' },
    { type: 'tool', name: 'fs_read', ok: true, detail: 'x' },
    { type: 'delta', text: 'C' },
    { type: 'assistant', text: 'C' },
    { type: 'turn', reason: 'done', steps: 1, calls: 1 },
  ])),
);

await test('单个 user 事件只渲染一个用户气泡', ['只出现一次'], () => {
  const app = run([{ type: 'user', text: '只出现一次' }]);
  return rows(app, 'user').map((row) => row.children[1].textContent);
});

await test('排队时首个 turn 保持运行，第二个 turn 才空闲', {
  afterFirst: { state: '运行中', dot: 'dot busy', busy: '1' },
  afterSecond: { state: '空闲', dot: 'dot', busy: '0' },
}, () => {
  const app = createApp();
  app.emit({ type: 'user', text: '第一条' });
  app.emit({ type: 'user', text: '第二条' });
  app.emit({ type: 'turn', reason: 'done', steps: 0, calls: 0 });
  const afterFirst = {
    state: app.ids.state.textContent,
    dot: app.ids.dot.className,
    busy: app.ids.liveStrip.dataset.busy,
  };
  app.emit({ type: 'turn', reason: 'done', steps: 0, calls: 0 });
  const afterSecond = {
    state: app.ids.state.textContent,
    dot: app.ids.dot.className,
    busy: app.ids.liveStrip.dataset.busy,
  };
  return { afterFirst, afterSecond };
});

await test('发送请求失败保留草稿', {
  draft: '网络失败也不能丢',
  userBubbles: 0,
  sendDisabled: false,
  ariaBusy: 'false',
}, async () => {
  const app = createApp((url) => {
    if (url === '/api/say') return Promise.reject(new Error('模拟断网'));
    return new Promise(() => {});
  });
  app.ids.input.value = '网络失败也不能丢';
  await app.ids.send.onclick();
  return {
    draft: app.ids.input.value,
    userBubbles: rows(app, 'user').length,
    sendDisabled: app.ids.send.disabled,
    ariaBusy: app.ids.input.getAttribute('aria-busy'),
  };
});

await test('escapeHtml 转义属性中的全部特殊字符', 'a&amp;&lt;&gt;&quot;&#39;', () => {
  const app = createApp();
  app.emit({ type: 'decision', callId: 'a&<>"\'', tool: 'fs_read', reason: 'x', seq: 1 });
  const match = app.ids.decisionList.innerHTML.match(/data-call-id="([^"]*)"/);
  return match ? match[1] : null;
});

await test('UU 状态只更新 .uu，不碰状态灯 .dot', {
  uuMoods: ['thinking', 'thinking'],
  dotClass: 'dot busy',
  dotMood: 'server-owned',
  selectors: ['.uu'],
}, checkUU);

process.exitCode = fail;
