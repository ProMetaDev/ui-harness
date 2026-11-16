// 通用 OpenAI 兼容适配器：内核把快照从 stdin 喂进来，这里翻译成 HTTP。
//
// 配置（按顺序找）：
//   1. 环境变量：UI_CHAT_BASE / UI_CHAT_KEY / UI_MODEL
//   2. 文件：secrets/astra.json（可用 UI_ASTRA_FILE 指定别的路径）
//
// 输入（stdin 一行 JSON）：{"prompt": "...", "snapshotId": "..."}
// 输出（stdout 一行 JSON）：
//   {"content": "...", "toolCalls": [], "usageTokens": 0}
//
// 不打印任何日志到 stdout；错误走 stderr + 非零退出码。

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
  const base = process.env.UI_CHAT_BASE || cfg.base_url || '';
  const key = process.env.UI_CHAT_KEY || cfg.api_key || '';
  const model = process.env.UI_MODEL || cfg.model || '';
  const chatPath = process.env.UI_CHAT_PATH || cfg.path || '/chat/completions';
  return { base, key, model, chatPath, file };
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
if (!cfg.base) die(`缺少 base_url：请填 ${cfg.file} 的 base_url（或用 UI_CHAT_BASE）`);
if (!cfg.key) die(`缺少 api_key：请填 ${cfg.file} 的 api_key（或用 UI_CHAT_KEY）`);
if (!cfg.model) die(`缺少 model：请填 ${cfg.file} 的 model（或用 UI_MODEL）`);
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

const url = cfg.base.replace(/\/+$/, '') + cfg.chatPath;
const timeoutMs = Number(process.env.UI_CHAT_TIMEOUT_MS || 600000);
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
    }),
  });
} catch (e) {
  clearTimeout(timer);
  die(`请求失败: ${e.message}`);
}
clearTimeout(timer);

const text = await res.text();
if (!res.ok) {
  die(`HTTP ${res.status}: ${text.slice(0, 500)}`);
}

let data;
try {
  data = JSON.parse(text);
} catch (e) {
  die(`响应不是 JSON: ${text.slice(0, 300)}`);
}

const choice = (data.choices && data.choices[0]) || {};
const content =
  (choice.message && choice.message.content) ||
  choice.text ||
  (data.output && JSON.stringify(data.output)) ||
  '';

const usage = data.usage || {};
const usageTokens = Number(usage.total_tokens || 0) || 0;

process.stdout.write(
  JSON.stringify({
    content,
    toolCalls: [],
    usageTokens,
  }) + '\n'
);
