// 通用 OpenAI 兼容适配器：内核把快照从 stdin 喂进来，这里翻译成 HTTP。
//
// 配置（按顺序找）：
//   1. 环境变量：UI_CHAT_BASE / UI_CHAT_KEY / UI_MODEL
//   2. 文件：secrets/astra.json（可用 UI_ASTRA_FILE 指定别的路径）
//
// 输入（stdin 一行 JSON）：{"snapshotId":..., "prompt": "...", "repair": null}
// 输出（stdout 一行 JSON）：{"content": "...", "toolCalls": [...], "usageTokens": 0}
//
// 两个关键点：
//   1. 工具协议在 prompt 里，模型输出 `{"content":...,"toolCalls":[...]}`，
//      适配器必须解析这个 envelope 再转发，否则内核看不到工具调用。
//   2. 中继会偶发 `fetch failed` / 5xx，必须重试——否则一次抖动就打断整条任务链。

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));

const PROTOCOL = `你是 ui 的后端模型。你的输出必须**只**是一个 JSON 对象，不要用 markdown 代码块包起来，也不要在 content 里再嵌一段 JSON。格式：

{"content": "给用户看的话", "toolCalls": [{"name": "工具名", "args": {参数}}]}

规则：
- content 用中文，简短说明你要做什么或结论。
- 需要动文件或跑命令时，放一个或多个 toolCalls；不调用工具时 toolCalls 用 []。
- 只能使用提示词里列出的工具名。
- 一次只做一件可验证的事；需要人决定时，在 content 里直接问，不要猜。`;

function readConfig() {
  const file = process.env.UI_ASTRA_FILE || path.join(HERE, '..', 'secrets', 'astra.json');
  let cfg = {};
  try {
    cfg = JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (e) {
    cfg = {};
  }
  return {
    base: process.env.UI_CHAT_BASE || cfg.base_url || '',
    key: process.env.UI_CHAT_KEY || cfg.api_key || '',
    model: process.env.UI_MODEL || cfg.model || '',
    chatPath: process.env.UI_CHAT_PATH || cfg.path || '/chat/completions',
    file,
  };
}

function die(message, code = 1) {
  process.stderr.write(message.endsWith('\n') ? message : message + '\n');
  process.exit(code);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function readStdin() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  return Buffer.concat(chunks).toString('utf8');
}

const cfg = readConfig();
if (!cfg.base) die(`缺少 base_url：请填 ${cfg.file} 的 base_url`);
if (!cfg.key) die(`缺少 api_key：请填 ${cfg.file} 的 api_key`);
if (!cfg.model) die(`缺少 model：请填 ${cfg.file} 的 model`);
if (cfg.key.includes('在此粘贴') || cfg.key.includes('PASTE')) {
  die(`api_key 还是占位符：请把真实 key 写进 ${cfg.file}`);
}

const raw = await readStdin();
let req;
try {
  req = JSON.parse(raw);
} catch (e) {
  die(`适配器输入不是 JSON: ${e.message}`);
}
const prompt = String(req.prompt || '');
if (!prompt) die('适配器输入里没有 prompt');

const messages = [
  { role: 'system', content: PROTOCOL },
  { role: 'user', content: prompt },
];
if (req.repair) {
  messages.push({
    role: 'user',
    content: `注意：上一次你的工具参数不是合法 JSON（${req.repair}）。请只输出一个合法的 JSON 对象。`,
  });
}

const stream = process.env.UI_CHAT_STREAM !== '0';
const url = cfg.base.replace(/\/+$/, '') + cfg.chatPath;
const timeoutMs = Number(process.env.UI_CHAT_TIMEOUT_MS || 900000);
const attempts = Number(process.env.UI_CHAT_RETRIES || 3);

async function once() {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(url, {
      method: 'POST',
      signal: controller.signal,
      headers: {
        'content-type': 'application/json',
        authorization: `Bearer ${cfg.key}`,
      },
      body: JSON.stringify({
        model: cfg.model,
        messages,
        temperature: 0.2,
        stream,
      }),
    });

    if (!res.ok) {
      const body = await res.text();
      const err = new Error(`HTTP ${res.status}: ${body.slice(0, 300)}`);
      // 4xx（除 429）不重试：重试也不会变好
      err.retryable = res.status >= 500 || res.status === 429;
      throw err;
    }

    let text = '';
    let usageTokens = 0;

    if (!stream) {
      const body = await res.text();
      let data;
      try {
        data = JSON.parse(body);
      } catch (e) {
        const err = new Error(`响应不是 JSON: ${body.slice(0, 200)}`);
        err.retryable = true;
        throw err;
      }
      const choice = (data.choices && data.choices[0]) || {};
      text = (choice.message && choice.message.content) || choice.text || '';
      usageTokens = Number((data.usage || {}).total_tokens || 0) || 0;
    } else {
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buf = '';
      let done = false;
      while (!done) {
        const { value, done: finished } = await reader.read();
        if (finished) break;
        buf += decoder.decode(value, { stream: true });
        let idx;
        while ((idx = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, idx).trim();
          buf = buf.slice(idx + 1);
          if (!line.startsWith('data:')) continue;
          const payload = line.slice(5).trim();
          if (payload === '[DONE]') {
            done = true;
            break;
          }
          try {
            const evt = JSON.parse(payload);
            const choice = (evt.choices && evt.choices[0]) || {};
            const delta = choice.delta || {};
            if (typeof delta.content === 'string') text += delta.content;
            else if (typeof choice.text === 'string') text += choice.text;
            if (evt.usage && evt.usage.total_tokens) {
              usageTokens = Number(evt.usage.total_tokens) || usageTokens;
            }
          } catch (e) {
            // 心跳或非 JSON 行
          }
        }
      }
    }

    if (!text) {
      const err = new Error('模型返回了空内容');
      err.retryable = true;
      throw err;
    }
    return { text, usageTokens };
  } finally {
    clearTimeout(timer);
  }
}

let result = null;
let lastErr = null;
for (let i = 0; i < attempts; i++) {
  try {
    result = await once();
    break;
  } catch (e) {
    lastErr = e;
    const retryable = e.retryable !== false;
    if (!retryable || i === attempts - 1) break;
    process.stderr.write(`适配器：第 ${i + 1} 次失败（${e.message}），重试…\n`);
    await sleep(1500 * (i + 1));
  }
}
if (!result) die(`请求失败: ${lastErr && lastErr.message}`);

const { text } = result;
let usageTokens = result.usageTokens;

// 从模型输出里取出协议 envelope（容忍 ```json 围栏和前后废话）
function tryParse(s) {
  try {
    const v = JSON.parse(s);
    if (v && typeof v === 'object' && ('content' in v || 'toolCalls' in v)) return v;
  } catch (e) {
    /* 不是 JSON */
  }
  return null;
}

function extractEnvelope(s) {
  const cleaned = s.replace(/^```(?:json)?\s*/i, '').replace(/```\s*$/, '').trim();
  const direct = tryParse(cleaned);
  if (direct) return direct;
  const first = cleaned.indexOf('{');
  const last = cleaned.lastIndexOf('}');
  if (first >= 0 && last > first) {
    const sliced = tryParse(cleaned.slice(first, last + 1));
    if (sliced) return sliced;
  }
  return null;
}

const envelope = extractEnvelope(text);
if (!envelope) {
  // 没按协议输出：把原文交回去，但**必须**标记协议错误。
  // 内核看到 protocolError 会要求重写（有上限），而不是把散文当成最终答案——
  // 否则一次截断/跑偏的模型输出看起来就像「任务已完成」。
  process.stdout.write(
    JSON.stringify({
      content: text,
      toolCalls: [],
      protocolError: '模型没有按 JSON 协议输出',
      usageTokens,
    }) + '\n'
  );
  process.exit(0);
}

const calls = Array.isArray(envelope.toolCalls) ? envelope.toolCalls : [];
const out = calls.map((c) => {
  const name = String((c && c.name) || '');
  const args = c && c.args;
  // 字符串形式的 args 原样交给内核：由内核严格解析，坏了走 model/repair。
  // 不能在这里换成 {}——那等于把「参数丢了」伪装成「工具不需要参数」。
  if (typeof args === 'string') return { name, argsRaw: args };
  return { name, args: args && typeof args === 'object' ? args : {} };
});

if (!usageTokens) usageTokens = Math.round((prompt.length + text.length) / 4);
process.stdout.write(
  JSON.stringify({
    content: String(envelope.content || ''),
    toolCalls: out,
    usageTokens,
  }) + '\n'
);
