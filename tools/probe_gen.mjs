// 看清可用模型 + 测一次真实出图（长超时），报告状态与耗时。
import fs from 'node:fs';
import https from 'node:https';
import { URL } from 'node:url';

const FILE = 'X:\\dsh_ws\\harness-v2\\secrets\\providers.toml';

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

const cfg = toml(fs.readFileSync(FILE, 'utf8')).gpt2image ?? {};
const key = cfg.api_key ?? '';
const base = (cfg.base_url ?? '').replace(/\/$/, '');
const model = cfg.model || 'gpt-image-2';

function req(method, url, body, timeoutMs) {
  return new Promise((resolve) => {
    const started = Date.now();
    const u = new URL(url);
    const payload = body ? Buffer.from(JSON.stringify(body)) : null;
    const headers = { authorization: `Bearer ${key}` };
    if (payload) {
      headers['content-type'] = 'application/json';
      headers['content-length'] = payload.length;
    }
    const r = https.request(
      { method, hostname: u.hostname, port: u.port || 443, path: u.pathname + u.search, timeout: timeoutMs, headers },
      (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          let t = Buffer.concat(chunks).toString('utf8');
          if (key) t = t.split(key).join('***');
          resolve({
            status: res.statusCode,
            ms: Date.now() - started,
            server: res.headers['server'] || '',
            ray: (res.headers['cf-ray'] || '').toString().slice(0, 12),
            body: t,
          });
        });
      },
    );
    r.on('timeout', () => { r.destroy(); resolve({ status: 0, ms: Date.now() - started, body: 'client timeout' }); });
    r.on('error', (e) => resolve({ status: 0, ms: Date.now() - started, body: `err ${e.code || e.message}` }));
    if (payload) r.write(payload);
    r.end();
  });
}

const models = await req('GET', `${base}/v1/models`, null, 20000);
console.log(`GET /v1/models -> ${models.status} ${models.ms}ms`);
try {
  const list = JSON.parse(models.body).data ?? [];
  for (const m of list) console.log(`  - ${m.id}  (${m.type ?? '?'}, ${m.owned_by ?? '?'}) ${m.display_name ?? ''}`);
} catch {
  console.log('  body:', models.body.slice(0, 300));
}

const body = { model, prompt: 'a single small blue square on white background', n: 1, size: '1024x1024' };
console.log(`\nPOST /v1/images/generations  model=${model}  (timeout 240s)`);
const r = await req('POST', `${base}/v1/images/generations`, body, 240000);
console.log(`-> ${r.status}  ${r.ms}ms  server=${r.server} ray=${r.ray}`);
console.log('  body:', r.body.slice(0, 300).replace(/\s+/g, ' '));
