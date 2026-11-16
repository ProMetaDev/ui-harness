import fs from 'node:fs';

// doctor：把沙箱状态如实打出来
const M = 'X:/dsh_ws/harness-v2/ui-cli/src/main.rs';
let m = fs.readFileSync(M, 'utf8');
const oldBlock = `    println!("  shell   ：获得批准后，命令内部的读写/联网不受效果策略约束");
    println!("            但执行前后会对比工作区，把**实际改动**记入 effect/observed（UI_OBSERVE=0 可关）");`;
const newBlock = `    // 沙箱状态：这是"能不能阻止"和"只能事后记账"的分界线，必须如实打
    let sb = ui_core::sandbox::describe(&ui_core::sandbox::Config::default());
    println!("  shell   ：{}", sb);
    println!("            执行前后还会对比工作区，把**实际改动**记入 effect/observed（UI_OBSERVE=0 可关）");
    #[cfg(not(target_os = "linux"))]
    println!("            注意：Windows 没有对等机制，shell 在那里仍是「可执行程序」级信任");`;
if (m.includes(oldBlock)) {
  m = m.replace(oldBlock, newBlock);
  fs.writeFileSync(M, m, 'utf8');
  console.log('doctor 已更新');
} else {
  console.log('锚点没找到（可能已改过）');
}

// 门禁加沙箱检查
const D = 'X:/dsh_ws/harness-v2/tools/debug.sh';
let d = fs.readFileSync(D, 'utf8');
if (!d.includes('verify_sandbox.sh')) {
  const anchor = 'run "观测式效果（shell 实际改动入日志）" bash tools/verify_observe.sh';
  const i = d.indexOf(anchor);
  if (i < 0) { console.error('门禁锚点没找到'); process.exit(1); }
  const nl = d.indexOf('\n', i);
  d = d.slice(0, nl + 1) + 'run "OS 沙箱（Landlock：区外写入被拒）" bash tools/verify_sandbox.sh\n' + d.slice(nl + 1);
  fs.writeFileSync(D, d, 'utf8');
  console.log('门禁已加沙箱检查');
}
