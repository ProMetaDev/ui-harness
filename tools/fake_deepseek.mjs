// 离线假的 DeepSeek 服务：只为验证适配器的流式协议，不联网、不花钱。
//   POST /chat/completions  body.stream=true  → SSE（data: {...}\n\n，[DONE] 结束）
//   POST /chat/completions  body.stream=false → 整段 JSON
//
// 模型文本本身是 UI 的协议 JSON，会被切成几段增量发出去——和真实模型一样：
// delta 是原始 JSON 文本，适配器必须收完整段才能解析。
import fs from 'node:fs';
import https from 'node:https';

const PORT = Number(process.env.FAKE_PORT || 18443);
const key = fs.readFileSync(process.env.FAKE_KEY);
const cert = fs.readFileSync(process.env.FAKE_CERT);

// 完整的「模型回复」文本：一个协议 JSON 对象
const TEXT = JSON.stringify({ content: '流式你好', toolCalls: [] });
// 按真实模型的粒度切碎：适配器要能把 content 逐段取出来，而不是把 JSON 括号推给界面
const CHUNKS = ['{"content":"', '流', '式', '你好', '","toolCalls":[]}'];

function readBody(req) {
  return new Promise((resolve) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
  });
}

function sse(res, body) {
  res.writeHead(200, {
    'content-type': 'text/event-stream',
    'cache-control': 'no-store',
    connection: 'keep-alive',
  });
  let i = 0;
  const tick = () => {
    if (i < CHUNKS.length) {
      const event = {
        choices: [{ index: 0, delta: { content: CHUNKS[i] }, finish_reason: null }],
      };
      res.write(`data: ${JSON.stringify(event)}\n\n`);
      i += 1;
      setTimeout(tick, 15);
      return;
    }
    if (i === CHUNKS.length) {
      const last = {
        choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
        usage: { prompt_tokens: 4, completion_tokens: 8, total_tokens: 12 },
      };
      res.write(`data: ${JSON.stringify(last)}\n\n`);
      i += 1;
      setTimeout(tick, 5);
      return;
    }
    res.write('data: [DONE]\n\n');
    res.end();
  };
  tick();
}

const server = https.createServer({ key, cert }, async (req, res) => {
  if (req.method !== 'POST' || !req.url.startsWith('/chat/completions')) {
    res.writeHead(404);
    res.end();
    return;
  }
  let body = {};
  try { body = JSON.parse(await readBody(req)); } catch { /* 忽略 */ }
  if (body.stream) {
    sse(res, body);
  } else {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({
      choices: [{ message: { content: TEXT }, finish_reason: 'stop' }],
      usage: { total_tokens: 12 },
    }));
  }
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`fake deepseek listening ${PORT}`);
});
