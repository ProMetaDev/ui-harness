import fs from 'node:fs';

// 1) doctor：Windows 那段改成实话（有 Job Object，但没有文件写入限制）
const M = 'X:/dsh_ws/harness-v2/ui-cli/src/main.rs';
let m = fs.readFileSync(M, 'utf8');
m = m.replace(
  '    #[cfg(not(target_os = "linux"))]\n    println!("            注意：Windows 没有对等机制，shell 在那里仍是「可执行程序」级信任");',
  '    #[cfg(windows)]\n    println!("            注意：Windows 只有 Job Object（进程树 + 资源上限），**没有**文件写入限制");'
);
m = m.replace(
  'println!("  结论    ：shell 在 Linux 上有写入沙箱；plugin / MCP 没有，仍按「可执行程序」对待");',
  'println!("  结论    ：Linux 上 shell 有写入沙箱；Windows 上只有资源与回收约束；plugin / MCP 两者都没有");'
);
fs.writeFileSync(M, m, 'utf8');
console.log('doctor 已更新（Windows 段）');

// 2) README：边界段补 Windows 现状
const R = 'X:/dsh_ws/harness-v2/README.md';
let r = fs.readFileSync(R, 'utf8');
r = r.replace(
  '- **Windows 上没有对等机制。** 那里 shell 仍是"可执行程序"级信任，只靠策略拦。',
  '- **Windows 上没有文件写入隔离。** 但有 Job Object：进程树由内核强制回收、内存与进程数有上限，\n  命令正常结束时也会连同后台子进程一起收掉（`start /b` 留下的孤儿不会活过命令）。'
);
fs.writeFileSync(R, r, 'utf8');
console.log('README 已更新');

// 3) SECURITY.md
const S = 'X:/dsh_ws/harness-v2/SECURITY.md';
let s = fs.readFileSync(S, 'utf8');
s = s.replace(
  '| Windows 上的 OS 隔离 | 没有 Job Object 限制。Linux 有 Landlock，Windows 没有对等物。 |',
  '| Windows 的文件隔离 | 只有 Job Object（进程/资源/回收），**没有**文件写入限制。 |'
);
fs.writeFileSync(S, s, 'utf8');
console.log('SECURITY.md 已更新');

// 4) docs/25：A 档标记完成，写实测结论
const P = 'X:/dsh_ws/harness-v2/docs/25-windows-sandbox-plan.md';
let p = fs.readFileSync(P, 'utf8');
p = p.replace(
  '## A 档：Job Object（半天，先做）',
  '## A 档：Job Object（✅ 已完成，v0.1.4）'
);
p = p.replace(
  '**验收**：',
  `**实测（2026-11-17，Windows 真机）**：

\`\`\`
=== A: timeout kills the tree ===
ping during run=1 (before=0)
ping after exit=0
PASS child ran (1) and the whole tree was reclaimed

=== B: normal exit must not orphan (start /b) ===
ping before=0 after=0
PASS no orphan (job terminated on completion)
\`\`\`

过程中发现一个真问题：**只靠 \`KILL_ON_JOB_CLOSE\` 不够**——命令正常结束（\`start /b\`
立刻返回）时，后台子进程会活下来。改成**正常结束时也显式 \`TerminateJobObject\`** 后，
B 才通过。回归脚本：\`tools/verify_job_windows.ps1\`。

**验收**：`
);
fs.writeFileSync(P, p, 'utf8');
console.log('docs/25 已更新');

// 5) 版本 0.1.3 → 0.1.4
const C = 'X:/dsh_ws/harness-v2/Cargo.toml';
let c = fs.readFileSync(C, 'utf8');
if (c.includes('version = "0.1.3"')) {
  c = c.replace('version = "0.1.3"', 'version = "0.1.4"');
  fs.writeFileSync(C, c, 'utf8');
  console.log('版本 → 0.1.4');
}
