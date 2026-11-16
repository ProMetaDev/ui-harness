// 出图工具：读 secrets/providers.toml → 调图片接口 → 存 PNG。
// 约定：绝不打印 api_key；错误体先擦掉 key 再截断；大响应只报字节数。
import fs from 'node:fs';
import https from 'node:https';
import { URL } from 'node:url';

const FILE = 'X:\\dsh_ws\\harness-v2\\secrets\\providers.toml';
const OUT_DIR = 'X:\\dsh_ws\\harness-v2\\out';

function parseTomlSimple(text) {
  const out = {};
  let section = null;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.replace(/#.*$/, '').trim();
    if (!line) continue;
    const sec = line.match(/^\[(.+)\]$/);
    if (sec) { section = sec[1]; out[section] ??= {}; continue; }
    const kv = line.match(/^([A-Za-z0-9_]+)\s*=\s*"(.*)"$/);
    if (kv && section) out[section][kv[1]] = kv[2];
  }
  return out;
}

function scrub(text, secret) {
  let t = String(text ?? '');
  if (secret) t = t.split(secret).join('***');
  return t.length > 400 ? t.slice(0, 400) + '…' : t;
}

function post(url, headers, body, timeoutMs = 150000) {
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
        headers: { 'content-type': 'application/json', 'content-length': payload.length, ...headers },
      },
      (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          const text = Buffer.concat(chunks).toString('utf8');
          let json = null;
          try { json = JSON.parse(text); } catch { /* not json */ }
          resolve({ status: res.statusCode, json, text });
        });
      },
    );
    req.on('timeout', () => { req.destroy(); resolve({ status: 0, json: null, text: 'timeout' }); });
    req.on('error', (e) => resolve({ status: 0, json: null, text: `error: ${e.code || e.message}` }));
    req.write(payload);
    req.end();
  });
}

const args = process.argv.slice(2);
const get = (k, d) => {
  const i = args.indexOf(`--${k}`);
  return i >= 0 && args[i + 1] ? args[i + 1] : d;
};
const has = (k) => args.includes(`--${k}`);

const cfg = parseTomlSimple(fs.readFileSync(FILE, 'utf8'));
const img = cfg.gpt2image ?? {};
const key = img.api_key;
if (!key) { console.error('api_key 为空'); process.exit(2); }
const base = (img.base_url || '').replace(/\/$/, '');
const model = img.model || 'gpt-image-2';
// 头：默认 Bearer；若配了 auth_header（"name: value"，可用 {key} 占位）就照它发。
const headers = {};
if (img.auth_header) {
  const i = img.auth_header.indexOf(':');
  const name = img.auth_header.slice(0, i).trim().toLowerCase();
  const value = img.auth_header.slice(i + 1).trim().replace('{key}', key);
  headers[name] = value;
} else {
  headers.authorization = `Bearer ${key}`;
}

const candidates = [
  `${base}/v1/images/generations`,
  `${base}/images/generations`,
  `${base}/v1/images/generate`,
];

if (has('probe')) {
  for (const url of candidates) {
    const r = await post(url, headers, { model });
    const hint = r.status === 404 ? '（路径不存在）' : r.status === 401 || r.status === 403 ? '（鉴权问题）' : '（路径存在）';
    console.log(`${r.status || r.text}  ${url} ${hint}`);
    if (r.status !== 404) console.log('   body:', scrub(r.text, key));
  }
  process.exit(0);
}

const promptArg = get('prompt');
const promptFile = get('prompt-file');
const prompt = promptFile ? fs.readFileSync(promptFile, 'utf8').trim() : promptArg;
if (!prompt) { console.error('缺少 --prompt 或 --prompt-file'); process.exit(2); }
const size = get('size', '1024x1024');
const n = Number(get('n', '1'));
const name = get('name', `image-${Date.now()}`);

let used = null;
let last = null;
for (const url of candidates) {
  const r = await post(url, headers, { model, prompt, n, size });
  last = r;
  if (r.status !== 404) { used = url; break; }
}
if (!used) {
  console.error('所有候选端点都 404：', last ? scrub(last.text, key) : 'no response');
  process.exit(3);
}
if (last.status !== 200) {
  console.error(`HTTP ${last.status} @ ${used}`);
  console.error(scrub(last.text, key));
  process.exit(4);
}

const data = last.json?.data;
if (!Array.isArray(data) || data.length === 0) {
  console.error('响应里没有 data[]:', scrub(last.text, key));
  process.exit(5);
}

fs.mkdirSync(OUT_DIR, { recursive: true });
const saved = [];
for (let i = 0; i < data.length; i++) {
  const item = data[i];
  const file = `${OUT_DIR}\\${name}${data.length > 1 ? `-${i + 1}` : ''}.png`;
  if (item.b64_json) {
    fs.writeFileSync(file, Buffer.from(item.b64_json, 'base64'));
    saved.push(file);
  } else if (item.url) {
    const buf = await new Promise((resolve, reject) => {
      https.get(item.url, (res) => {
        const c = [];
        res.on('data', (d) => c.push(d));
        res.on('end', () => resolve(Buffer.concat(c)));
      }).on('error', reject);
    });
    fs.writeFileSync(file, buf);
    saved.push(file);
  } else {
    console.error('未知的 data 形状:', Object.keys(item));
  }
}

console.log(`endpoint: ${used}`);
console.log(`model   : ${model}`);
console.log(`size    : ${size}`);
for (const f of saved) console.log(`saved   : ${f} (${fs.statSync(f).size} bytes)`);
