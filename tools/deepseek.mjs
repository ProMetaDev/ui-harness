// DeepSeek 适配器：Rust 内核把快照从 stdin 喂进来，这里只负责「翻译成 HTTP」。
//
//   输入  {"prompt": "...", "repair": null|"上一次参数错在哪", "snapshotId": "...", "stream": true|false}
//   输出（流式，默认）：
//     {"delta":"<增量文本>"}          ← 每行一个，仅用于展示
//     ...
//     {"content":...,"toolCalls":[...],"usageTokens":...}   ← 最后一行才是最终结果
//   输出（非流式：input.stream=false 或 UI_STREAM=0）：只有最后那一行。
//
// 注意：模型被要求输出 JSON 协议，流式时 delta 是**原始 JSON 文本**，
// 不能当成答案；必须等整段文本收完再 parseProtocol。
// 不在 stdout 打印任何日志；错误走 stderr + 非零退出码，由内核记成 model/error。
import fs from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import { URL, fileURLToPath } from 'node:url';

// 相对脚本定位：同一份代码在 Windows 和 WSL 下都能找到密钥文件。
// UI_PROVIDERS_FILE 只给离线探针用：让它指向测试配置，不碰真实密钥。
const HERE = path.dirname(fileURLToPath(import.meta.url));
const FILE = process.env.UI_PROVIDERS_FILE || path.join(HERE, '..', 'secrets', 'providers.toml');

function toml(text) {
  const out = {};
  let s = null;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.replace(/#.*$/, '').trim();
    if (!line) continue;
    const sec = line.match(/^\[(.+)\]$/);
    if (sec) { s = sec[1]; out[s] ??= {}; continue; }
    const kv = line.match(/^([A-Za-z0-9_]+)\s*=\s*"(.*)"$/);
    if (kv && s) out[s][kv[1]] = kv[2];
  }
  return out;
}

const cfg = toml(fs.readFileSync(FILE, 'utf8')).chat ?? {};
const key = cfg.api_key || process.env.DEEPSEEK_API_KEY || '';
const base = (cfg.base_url || 'https://api.deepseek.com').replace(/\/$/, '');
const model = cfg.model || 'deepseek-chat';

const PROTOCOL = `你在一个名为 UI 的 harness 里工作。

你只能输出**一个** JSON 对象，不要 Markdown 代码围栏，不要解释，不要输出思考过程，
也不要在 content 里再嵌一段 JSON。格式：

{"content": "给用户看的话", "toolCalls": [{"name": "工具名", "args": {参数}}]}

规则：
- 需要动文件或跑命令时，放一个或多个 toolCalls；不调用工具时 toolCalls 用 []。
- 工具的可用参数见提示里的 <tools> 段，不要发明不存在的工具或参数。
- 一次只做一件可验证的事；需要人决定时，content 里直接问，不要猜。
- 不要假装成功：只有工具返回 ok 才能说完成。

记住：你的**整条回复**必须是一个 JSON 对象，不能有别的字符。`;

function readStdin() {
  return new Promise((resolve, reject) => {
    const chunks = [];
    process.stdin.on('data', (c) => chunks.push(c));
    process.stdin.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    process.stdin.on('error', reject);
  });
}

function post(url, headers, body, timeoutMs = 180000) {
  return new Promise((resolve) => {
    const u = new URL(url);
    const payload = Buffer.from(JSON.stringify(body));
    const req = https.request(
      {
        method: 'POST',
        hostname: u.hostname,
        port: u.port || 443,
        path: u.pathname + u.search,
        timeout: timeoutMs,
        headers: { ...headers, 'content-type': 'application/json', 'content-length': payload.length },
      },
      (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          const text = Buffer.concat(chunks).toString('utf8');
          let json = null;
          try { json = JSON.parse(text); } catch { /* 非 JSON 错误页 */ }
          resolve({ status: res.statusCode, json, text });
        });
      },
    );
    req.on('timeout', () => { req.destroy(); resolve({ status: 0, json: null, text: 'timeout' }); });
    req.on('error', (e) => resolve({ status: 0, json: null, text: String(e.code || e.message) }));
    req.write(payload);
    req.end();
  });
}

/**
 * OpenAI 兼容的流式 POST：自己解 SSE（`data: {...}` 行，`[DONE]` 结束），
 * 每收到一个 content 增量就回调 onDelta；返回值伪装成非流式响应，好复用后面的协议解析。
 */
function postStream(url, headers, body, onDelta, timeoutMs = 180000) {
  return new Promise((resolve) => {
    const u = new URL(url);
    const payload = Buffer.from(JSON.stringify(body));
    let settled = false;
    const finish = (value) => { if (!settled) { settled = true; resolve(value); } };

    let content = '';
    let reasoning = '';
    let finishReason = null;
    let usage = null;
    let buf = '';

    function feed(rawLine) {
      const line = rawLine.replace(/\r$/, '');
      if (!line.startsWith('data:')) return;
      const data = line.slice(5).trim();
      if (!data) return;
      if (data === '[DONE]') return;
      let v;
      try { v = JSON.parse(data); } catch { return; /* 心跳/非 JSON 行 */ }
      if (v.usage) usage = v.usage;
      const choice = v.choices?.[0];
      if (!choice) return;
      if (choice.finish_reason) finishReason = choice.finish_reason;
      const d = choice.delta ?? {};
      if (typeof d.reasoning_content === 'string') reasoning += d.reasoning_content;
      if (typeof d.content === 'string' && d.content) {
        content += d.content;
        onDelta(d.content);
      }
    }

    const req = https.request(
      {
        method: 'POST',
        hostname: u.hostname,
        port: u.port || 443,
        path: u.pathname + u.search,
        timeout: timeoutMs,
        headers: { ...headers, 'content-type': 'application/json', 'content-length': payload.length },
      },
      (res) => {
        if (res.statusCode !== 200) {
          const chunks = [];
          res.on('data', (c) => chunks.push(c));
          res.on('end', () => finish({
            status: res.statusCode,
            json: null,
            text: Buffer.concat(chunks).toString('utf8'),
          }));
          return;
        }
        res.setEncoding('utf8');
        res.on('data', (chunk) => {
          buf += chunk;
          let idx;
          while ((idx = buf.indexOf('\n')) >= 0) {
            const line = buf.slice(0, idx);
            buf = buf.slice(idx + 1);
            feed(line);
          }
        });
        res.on('end', () => {
          if (buf.trim()) feed(buf);
          finish({
            status: res.statusCode,
            json: {
              choices: [{
                message: {
                  content,
                  ...(reasoning ? { reasoning_content: reasoning } : {}),
                },
                finish_reason: finishReason,
              }],
              ...(usage ? { usage } : {}),
            },
            text: '',
          });
        });
        res.on('error', (e) => finish({ status: 0, json: null, text: String(e.code || e.message) }));
      },
    );
    req.on('timeout', () => { req.destroy(); finish({ status: 0, json: null, text: 'timeout' }); });
    req.on('error', (e) => finish({ status: 0, json: null, text: String(e.code || e.message) }));
    req.write(payload);
    req.end();
  });
}

function stripFences(s) {
  let t = String(s || '').trim();
  if (t.startsWith('```')) {
    t = t.split('\n').filter((l) => !l.trimStart().startsWith('```')).join('\n');
  }
  return t.trim();
}

/** 把文本里所有平衡的 {...} 切出来（忽略字符串内的括号）。 */
function balancedObjects(text) {
  const out = [];
  let depth = 0;
  let start = -1;
  let inStr = false;
  let esc = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (inStr) {
      if (esc) esc = false;
      else if (c === '\\') esc = true;
      else if (c === '"') inStr = false;
      continue;
    }
    if (c === '"') { inStr = true; continue; }
    if (c === '{') { if (depth === 0) start = i; depth += 1; }
    else if (c === '}') {
      depth -= 1;
      if (depth === 0 && start >= 0) { out.push(text.slice(start, i + 1)); start = -1; }
    }
  }
  return out;
}

/** 协议解析：整段 → 从后往前的平衡对象。模型有时会把两轮输出拼在一起。 */
function parseProtocol(raw) {
  const t = stripFences(raw);
  const candidates = [t, ...balancedObjects(t).reverse()];
  for (const c of candidates) {
    try {
      const v = JSON.parse(c);
      if (v && typeof v === 'object' && ('content' in v || 'toolCalls' in v)) return { value: v, salvaged: c !== t };
    } catch { /* 下一个 */ }
  }
  return null;
}

const raw = await readStdin();
let input;
try {
  input = JSON.parse(raw);
} catch (e) {
  console.error('适配器收到的输入不是 JSON:', e.message);
  process.exit(2);
}

const messages = [
  { role: 'system', content: PROTOCOL },
  { role: 'user', content: String(input.prompt || '') },
];
if (input.repair) {
  messages.push({
    role: 'user',
    content: '注意：上一次你的工具参数不是合法 JSON（' + input.repair +
      '）。这次请输出完整、合法的 JSON 对象。',
  });
}

// 默认流式；调用方显式传 stream:false 或设 UI_STREAM=0 时退回一次性输出。
const stream = input.stream !== false && process.env.UI_STREAM !== '0';

const body = {
  model,
  messages,
  temperature: 0.2,
  max_tokens: 3000,
  stream,
  ...(stream ? { stream_options: { include_usage: true } } : {}),
  response_format: { type: 'json_object' },
};

if (process.argv.includes('--dry')) {
  console.log(JSON.stringify({ url: `${base}/chat/completions`, model, messages }, null, 2));
  process.exit(0);
}

if (!key) {
  console.error('没有 chat key：请填 secrets/providers.toml 的 [chat].api_key');
  process.exit(3);
}

function writeDelta(text) {
  // 一行一个 JSON，增量里的换行会被 JSON.stringify 转义，不会破坏分行。
  process.stdout.write(JSON.stringify({ delta: text }) + '\n');
}

/**
 * 从「累积的协议 JSON 文本」里尽力取出 `content` 字符串的当前值。
 * 收不全（转义没结束、\uXXXX 没到齐）就返回已经能确定的部分。
 */
function extractContent(raw) {
  const key = /"content"\s*:\s*"/.exec(raw);
  if (!key) return null;
  let i = key.index + key[0].length;
  let out = '';
  while (i < raw.length) {
    const ch = raw[i];
    if (ch === '\\') {
      const next = raw[i + 1];
      if (next === undefined) break; // 转义还没收完，等下一段
      if (next === 'u') {
        if (i + 6 > raw.length) break;
        const hex = raw.slice(i + 2, i + 6);
        if (!/^[0-9a-fA-F]{4}$/.test(hex)) break;
        out += String.fromCharCode(parseInt(hex, 16));
        i += 6;
        continue;
      }
      const map = { n: '\n', t: '\t', r: '\r', '"': '"', '\\': '\\', '/': '/' };
      out += map[next] ?? next;
      i += 2;
      continue;
    }
    if (ch === '"') break; // content 结束
    out += ch;
    i += 1;
  }
  return out;
}

/**
 * 流式展示用：模型输出的是协议 JSON，直接把原文推给界面会看到 `{"content":"…`。
 * 这里只把 content 字段的**新增可见文本**推出去。
 */
function makeDeltaEmitter() {
  let acc = '';
  let shown = '';
  return (text) => {
    acc += text;
    const content = extractContent(acc);
    if (content === null) return false; // 还没看到 content 字段
    if (!content.startsWith(shown)) return false; // 形态变了，等下一段再判断
    const add = content.slice(shown.length);
    if (!add) return false;
    shown = content;
    writeDelta(add);
    return true;
  };
}


/** 按当前 mode 发一次请求；流式时记录是否已经吐出过 delta（决定能不能安全重试）。 */
async function call(requestBody) {
  let emitted = false;
  const emit = makeDeltaEmitter();
  const res = stream
    ? await postStream(
      `${base}/chat/completions`,
      { authorization: `Bearer ${key}` },
      requestBody,
      (text) => { if (emit(text)) emitted = true; },
    )
    : await post(`${base}/chat/completions`, { authorization: `Bearer ${key}` }, requestBody);
  res.emitted = emitted;
  return res;
}

let useBody = body;
let res = await call(useBody);

// 个别服务不认识 stream_options：还没吐过增量就退回不带它的请求。
if (stream && res.status === 400 && !res.emitted) {
  const { stream_options, ...fallback } = useBody;
  useBody = fallback;
  res = await call(useBody);
}

// 429 / 5xx 重试一次，别把偶发故障算到模型头上。
// 流式已经吐过 delta 时不再重试：画面会重复，而且最终结果也未必一致。
if ((res.status === 429 || (res.status >= 500 && res.status < 600)) && !(stream && res.emitted)) {
  await new Promise((r) => setTimeout(r, 1200));
  res = await call(useBody);
}

if (res.status !== 200 || !res.json) {
  let text = res.text;
  if (key) text = text.split(key).join('***');
  console.error(`HTTP ${res.status}: ${String(text).slice(0, 400)}`);
  process.exit(4);
}

// 有些模型会先把 token 花在 reasoning 上，`content` 于是是空的
// （finish_reason=length）。这不是"模型没话说"，是预算不够——放大 token 再试一次。
{
  const first = res.json.choices?.[0] ?? {};
  const empty = !(first.message?.content || '').trim();
  if (empty && (first.finish_reason === 'length' || first.message?.reasoning_content)) {
    res = await call({ ...useBody, max_tokens: 8000 });
    if (res.status !== 200 || !res.json) {
      console.error(`重试失败 HTTP ${res.status}`);
      process.exit(4);
    }
  }
}

const message = res.json.choices?.[0]?.message ?? {};
let content = message.content ?? '';
let toolCalls = [];
let protocolError = null;

// 流式时这里才拿到完整文本：delta 只用于展示，协议解析只看最终全文。
const parsed = parseProtocol(content);
if (parsed) {
  const v = parsed.value;
  content = typeof v.content === 'string' ? v.content : '';
  const list = Array.isArray(v.toolCalls) ? v.toolCalls : [];
  toolCalls = list.map((c) => ({
    name: String(c.name || ''),
    // args 允许是对象或字符串；字符串交给内核的抢救逻辑
    ...(typeof c.args === 'string' ? { argsRaw: c.args } : { args: c.args ?? {} }),
  }));
} else if (Array.isArray(message.tool_calls) && message.tool_calls.length) {
  // 模型走了原生 tool_calls，没走 JSON 协议
  toolCalls = message.tool_calls.map((c) => ({
    name: c.function?.name || '',
    argsRaw: c.function?.arguments ?? '{}',
  }));
} else {
  // 完全不是 JSON：不要当作最终答案。把原文带上并标记协议错误，
  // 由内核要求模型重写——静默接受散文等于假装任务完成了。
  content = stripFences(content);
  protocolError = '模型没有按 JSON 协议输出';
}

const usage = res.json.usage;
const out = {
  content,
  toolCalls,
  ...(protocolError ? { protocolError } : {}),
  usageTokens: usage && Number.isFinite(usage.total_tokens) ? usage.total_tokens : undefined,
};
// 最后一行才是结果；流式时它前面每行都是 {"delta":...}。
process.stdout.write(JSON.stringify(out) + '\n');
