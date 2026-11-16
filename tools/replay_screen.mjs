// 把 ANSI 流还原成「屏幕帧」，用来在没有终端的地方展示 TUI 实际长什么样。
// 用法: node tools/replay_screen.mjs /tmp/tui2.out [要打印几帧]
import fs from 'node:fs';

const file = process.argv[2];
const want = Number(process.argv[3] || 4);
const W = 80;
const H = 24;

const raw = fs.readFileSync(file, 'utf8');

function charWidth(cp) {
  if (cp === 0) return 0;
  if (cp >= 0x0300 && cp <= 0x036f) return 0;
  const wide =
    (cp >= 0x1100 && cp <= 0x115f) ||
    (cp >= 0x2e80 && cp <= 0x303e) ||
    (cp >= 0x3041 && cp <= 0x33ff) ||
    (cp >= 0x3400 && cp <= 0x4dbf) ||
    (cp >= 0x4e00 && cp <= 0x9fff) ||
    (cp >= 0xa000 && cp <= 0xa4cf) ||
    (cp >= 0xac00 && cp <= 0xd7a3) ||
    (cp >= 0xf900 && cp <= 0xfaff) ||
    (cp >= 0xfe30 && cp <= 0xfe4f) ||
    (cp >= 0xff00 && cp <= 0xff60) ||
    (cp >= 0xffe0 && cp <= 0xffe6) ||
    (cp >= 0x1f300 && cp <= 0x1f64f) ||
    (cp >= 0x20000 && cp <= 0x3fffd);
  return wide ? 2 : 1;
}

let screen = Array.from({ length: H }, () => Array(W).fill(' '));
let row = 0;
let col = 0;
const frames = [];
let touched = false;

function snapshot() {
  const rows = screen.map((r) => r.join('').replace(/\s+$/, ''));
  // 去掉尾部空行
  while (rows.length && rows[rows.length - 1] === '') rows.pop();
  frames.push(rows);
  screen = Array.from({ length: H }, () => Array(W).fill(' '));
  touched = false;
}

let i = 0;
while (i < raw.length) {
  const ch = raw[i];
  if (ch === '\x1b' && raw[i + 1] === '[') {
    let j = i + 2;
    while (j < raw.length && /[0-9;?]/.test(raw[j])) j += 1;
    const cmd = raw[j];
    const params = raw.slice(i + 2, j);
    if (cmd === 'H') {
      const [r, c] = params.split(';').map((v) => Number(v || 1));
      if (r === 1 && c === 1 && touched) snapshot();
      row = (r || 1) - 1;
      col = (c || 1) - 1;
    } else if (cmd === 'K') {
      for (let k = col; k < W; k += 1) screen[row][k] = ' ';
    } else if (cmd === 'J') {
      screen = Array.from({ length: H }, () => Array(W).fill(' '));
    } else if (cmd === 'l' && params === '?1049') {
      // 离开备用屏：之后是普通终端输出，不属于界面
      if (touched) snapshot();
      break;
    }
    i = j + 1;
    continue;
  }
  if (ch === '\r') { col = 0; i += 1; continue; }
  if (ch === '\n') { row = Math.min(H - 1, row + 1); i += 1; continue; }
  const cp = raw.codePointAt(i);
  const s = String.fromCodePoint(cp);
  i += s.length;
  if (row < 0 || row >= H || col < 0 || col >= W) continue;
  const w = charWidth(cp);
  if (col + w > W) continue;
  screen[row][col] = s;
  if (w === 2 && col + 1 < W) screen[row][col + 1] = '';
  col += w;
  touched = true;
}
if (touched) snapshot();

console.log(`frames: ${frames.length}  (${file})`);
const needle = process.argv[4];
let order;
if (needle) {
  order = frames.map((_, k) => k).filter((k) => frames[k].join('\n').includes(needle));
  console.log(`frames containing ${JSON.stringify(needle)}: ${order.length}`);
  order = order.slice(0, Math.max(1, want));
} else {
  const picks = new Set();
  if (want <= 1) {
    picks.add(frames.length - 1);
  } else if (frames.length <= want) {
    frames.forEach((_, k) => picks.add(k));
  } else {
    for (let k = 0; k < want; k += 1) {
      picks.add(Math.round((k * (frames.length - 1)) / (want - 1)));
    }
  }
  order = [...picks].sort((a, b) => a - b);
}
for (const k of order) {
  console.log(`\n───── frame ${k + 1}/${frames.length} ─────`);
  console.log(frames[k].join('\n'));
}
