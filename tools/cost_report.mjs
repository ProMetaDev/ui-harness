// 上下文成本报告 / 两次运行对比。
//   cost_report.mjs <journal.jsonl>
//   cost_report.mjs --compare <a.jsonl> <b.jsonl>
// 依据：每条 prompt/snapshot 带 sections[{id,tokens,bytes}]，tool/request 带工具名。
import fs from 'node:fs';

const args = process.argv.slice(2);
if (!args.length) {
  console.error('用法: cost_report.mjs <journal.jsonl> | --compare <a> <b>');
  process.exit(2);
}

function readJournal(file) {
  const lines = fs.readFileSync(file, 'utf8').split('\n').filter((l) => l.trim());
  const snaps = [];
  const tools = [];
  for (const line of lines) {
    let ev;
    try {
      ev = JSON.parse(line);
    } catch {
      continue;
    }
    if (ev.kind === 'prompt/snapshot') snaps.push(ev);
    if (ev.kind === 'tool/request') tools.push(ev.tool);
  }
  const total = snaps.reduce((s, x) => s + (x.tokens || 0), 0);
  const hist = (s) => (s.sections || []).find((x) => x.id === 'history')?.tokens || 0;
  return {
    file,
    snaps,
    tools,
    total,
    histFirst: snaps.length ? hist(snaps[0]) : 0,
    histLast: snaps.length ? hist(snaps[snaps.length - 1]) : 0,
  };
}

function report(j) {
  const cols = [...new Set(j.snaps.flatMap((s) => (s.sections || []).map((x) => x.id)))];
  const pad = (s, n = 12) => String(s).padEnd(n);
  console.log(pad('seq', 6) + pad('tokens') + cols.map((c) => pad(c)).join(''));
  for (const s of j.snaps) {
    const by = {};
    for (const sec of s.sections || []) by[sec.id] = (by[sec.id] || 0) + sec.tokens;
    console.log(pad(s.seq, 6) + pad(s.tokens) + cols.map((c) => pad(by[c] || 0)).join(''));
  }
  const last = j.snaps[j.snaps.length - 1] || { tokens: 1, sections: [] };
  const lastBy = {};
  for (const sec of last.sections || []) lastBy[sec.id] = (lastBy[sec.id] || 0) + sec.tokens;
  console.log('');
  console.log(`快照数          ${j.snaps.length}`);
  console.log(`快照 token 合计 ${j.total}`);
  console.log(`平均每步        ${j.snaps.length ? Math.round(j.total / j.snaps.length) : 0}`);
  console.log(
    `history 首→尾   ${j.histFirst} → ${j.histLast}  （每步 +${
      j.snaps.length > 1 ? Math.round((j.histLast - j.histFirst) / (j.snaps.length - 1)) : 0
    }）`,
  );
  // 每一步都在重发上一步的全部内容：这部分是"理论上可被前缀缓存吃掉"的量
  const resent = j.snaps.slice(1).reduce((s, x, i) => s + (j.snaps[i].tokens || 0), 0);
  console.log(
    `重发前缀总量    ${resent}  （占输入 ${((resent / (j.total || 1)) * 100).toFixed(0)}%）`,
  );
  console.log('最后一步占比');
  for (const [k, v] of Object.entries(lastBy)) {
    console.log(`  ${k.padEnd(10)} ${String(v).padStart(6)}  ${((v / (last.tokens || 1)) * 100).toFixed(0)}%`);
  }
}

if (args[0] === '--compare') {
  const a = readJournal(args[1]);
  const b = readJournal(args[2]);
  const fmt = (n) => String(n).padStart(8);
  console.log('指标                     A            B         差');
  console.log(
    `快照数        ${fmt(a.snaps.length)}   ${fmt(b.snaps.length)}   ${fmt(b.snaps.length - a.snaps.length)}`,
  );
  console.log(
    `输入 token    ${fmt(a.total)}   ${fmt(b.total)}   ${fmt(b.total - a.total)}`,
  );
  console.log(
    `每步 history  ${fmt(a.snaps.length > 1 ? Math.round((a.histLast - a.histFirst) / (a.snaps.length - 1)) : 0)}   ${fmt(
      b.snaps.length > 1 ? Math.round((b.histLast - b.histFirst) / (b.snaps.length - 1)) : 0,
    )}   -`,
  );
  console.log(`工具调用数    ${fmt(a.tools.length)}   ${fmt(b.tools.length)}   ${fmt(b.tools.length - a.tools.length)}`);

  // 第一次分叉：工具序列不同的位置
  let diverge = -1;
  for (let i = 0; i < Math.max(a.tools.length, b.tools.length); i++) {
    if (a.tools[i] !== b.tools[i]) {
      diverge = i;
      break;
    }
  }
  console.log('');
  if (diverge < 0) {
    console.log('两次的工具序列完全一致');
  } else {
    console.log(`第一次分叉：第 ${diverge + 1} 次工具调用`);
    console.log(`  A: ${a.tools[diverge] ?? '(无)'}`);
    console.log(`  B: ${b.tools[diverge] ?? '(无)'}`);
    const show = (j, from) => j.tools.slice(Math.max(0, from - 2), from + 3).join(' → ');
    console.log(`  A 附近: ${show(a, diverge)}`);
    console.log(`  B 附近: ${show(b, diverge)}`);
  }
} else {
  report(readJournal(args[0]));
}
