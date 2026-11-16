// 探测 [chat] 配置与 api.deepseek.com 可达性。只报状态，不回显 key。
import fs from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import { URL, fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FILE = path.join(HERE, '..', 'secrets', 'providers.toml');

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

const cfg = toml(fs.readFileSync(FILE, 'utf8'));
const chat = cfg.chat ?? {};
const key = chat.api_key || '';
const base = (chat.base_url || 'https://api.deepseek.com').replace(/\/$/, '');

console.log('[chat]');
console.log('  api_key :', key ? `<set, ${key.length} chars>` : '<empty>');
console.log('  base_url:', chat.base_url || '<empty, 将用 https://api.deepseek.com>');
console.log('  model   :', chat.model || '<empty, 将用 deepseek-chat>');

if (!key) {
  console.log('\n没有 chat key，先跳过网络探测。');
  process.exit(0);
}

function req(method, url, body) {
  return new Promise((resolve) => {
    const u = new URL(url);
    const payload = body ? Buffer.from(JSON.stringify(body)) : null;
    const headers = { authorization: `Bearer ${key}` };
    if (payload) {
      headers['content-type'] = 'application/json';
      headers['content-length'] = payload.length;
    }
    const r = https.request(
      { method, hostname: u.hostname, port: u.port || 443, path: u.pathname + u.search, timeout: 20000, headers },
      (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          let t = Buffer.concat(chunks).toString('utf8');
          if (key) t = t.split(key).join('***');
          resolve({ status: res.statusCode, body: t.slice(0, 200).replace(/\s+/g, ' ') });
        });
      },
    );
    r.on('timeout', () => { r.destroy(); resolve({ status: 0, body: 'timeout' }); });
    r.on('error', (e) => resolve({ status: 0, body: `err ${e.code || e.message}` }));
    if (payload) r.write(payload);
    r.end();
  });
}

const models = await req('GET', `${base}/models`);
console.log('\nGET /models ->', models.status, models.body);

const chatCall = await req('POST', `${base}/chat/completions`, {
  model: chat.model || 'deepseek-chat',
  messages: [{ role: 'user', content: 'reply with the single word: ok' }],
  max_tokens: 8,
  stream: false,
});
console.log('POST /chat/completions ->', chatCall.status, chatCall.body);
