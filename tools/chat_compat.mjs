// 通用 OpenAI 兼容适配器：内核把快照从 stdin 喂进来，这里翻译成 HTTP。
//
// 配置（按顺序找）：
//   1. 环境变量：UI_CHAT_BASE / UI_CHAT_KEY / UI_MODEL
//   2. 文件：secrets/astra.json（可用 UI_ASTRA_FILE 指定别的路径）
//
// 输入（stdin 一行 JSON）：{"prompt": "...", "snapshotId": "..."}
// 输出（stdout 一行 JSON）：{"content": "...", "toolCalls": [], "usageTokens": 0}
//
// 默认走**流式**：长回答时非流式会被网关判定空闲（实测 524）。
// 用 UI_CHAT_STREAM=0 可以退回非流式。

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));

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

const stream = process.env.UI_CHAT_STREAM !== '0';
const url = cfg.base.replace(/\/+$/, '') + cfg.chatPath;
const timeoutMs = Number(process.env.UI_CHAT_TIMEOUT_MS || 900000);
const controller = new AbortController();
const timer = setTimeout(() => controller.abort(), timeoutMs);

let res;
try {
  res = await fetch(url, {
    method: 'POST',
    signal: controller.signal,
    headers: {
      'content-type': 'application/json',
      authorization: `Bearer ${cfg.key}`,
    },
    body: JSON.stringify({
      model: cfg.model,
      messages: [{ role: 'user', content: prompt }],
      temperature: 0.3,
      stream,
    }),
  });
} catch (e) {
  clearTimeout(timer);
  die(`请求失败: ${e.message}`);
}

if (!res.ok) {
  const text = await res.text();
  clearTimeout(timer);
  die(`HTTP ${res.status}: ${text.slice(0, 500)}`);
}

let content = '';
let usageTokens = 0;

if (!stream) {
  const text = await res.text();
  clearTimeout(timer);
  let data;
  try {
    data = JSON.parse(text);
  } catch (e) {
    die(`响应不是 JSON: ${text.slice(0, 300)}`);
  }
  const choice = (data.choices && data.choices[0]) || {};
  content = (choice.message && choice.message.content) || choice.text || '';
  usageTokens = Number((data.usage || {}).total_tokens || 0) || 0;
} else {
  // 逐块读 SSE：保持连接有数据流动，避免网关空闲超时
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
        if (typeof delta.content === 'string') content += delta.content;
        else if (typeof choice.text === 'string') content += choice.text;
        if (evt.usage && evt.usage.total_tokens) {
          usageTokens = Number(evt.usage.total_tokens) || usageTokens;
        }
      } catch (e) {
        // 不是 JSON 的行（心跳、注释）直接跳过
      }
    }
  }
  clearTimeout(timer);
}

if (!content) die('模型返回了空内容');
if (!usageTokens) usageTokens = Math.round((prompt.length + content.length) / 4);

process.stdout.write(JSON.stringify({ content, toolCalls: [], usageTokens }) + '\n');
