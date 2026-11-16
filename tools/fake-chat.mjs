// 假的 chat 适配器：只为验证「内核 ↔ 子进程」这条缝真的通。
// 用调用次数决定回什么，跑完看 journal 就知道内核有没有正确执行工具。
import fs from 'node:fs';

const COUNTER = '/tmp/ui-fake-count';

function readStdin() {
  return new Promise((resolve) => {
    const chunks = [];
    process.stdin.on('data', (c) => chunks.push(c));
    process.stdin.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
  });
}

const input = JSON.parse(await readStdin());
fs.appendFileSync('/tmp/ui-fake-prompt.log', '\n=== 收到快照 ===\n' + String(input.prompt || '').slice(0, 400) + '\n');

const n = fs.existsSync(COUNTER) ? Number(fs.readFileSync(COUNTER, 'utf8')) : 0;
fs.writeFileSync(COUNTER, String(n + 1));

const replies = [
  {
    content: '先读一下 greet.js。',
    toolCalls: [{ name: 'fs_read', args: { path: 'greet.js' } }],
  },
  {
    content: '空名字会拼出 hi ，改成兜底。',
    toolCalls: [{
      name: 'fs_edit',
      args: {
        path: 'greet.js',
        old: 'return `hi ${name}`;',
        new: "return `hi ${name || 'there'}`;",
      },
    }],
  },
  { content: '改完了：空名字现在返回 hi there。' },
];

// 参数坏掉那次：模拟模型吐了半个 JSON，验证内核的抢救/修复
const broken = process.env.FAKE_BROKEN === '1' && n === 1;

process.stdout.write(JSON.stringify(broken
  ? { content: '', toolCalls: [{ name: 'fs_write', argsRaw: '{"path":"x.txt","content":' }] }
  : replies[Math.min(n, replies.length - 1)]));
