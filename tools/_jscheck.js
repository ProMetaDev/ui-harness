const fs = require('fs');
const P = 'X:/dsh_ws/harness-v2/ui-cli/assets/chat.html';
const html = fs.readFileSync(P, 'utf8');
const re = /<script>([\s\S]*?)<\/script>/g;
let m, i = 0, bad = 0;
while ((m = re.exec(html))) {
  i++;
  const code = m[1];
  try {
    new Function(code);
    console.log(`script #${i}: 语法 OK (${code.length} 字节)`);
  } catch (e) {
    bad++;
    console.log(`script #${i}: 语法错误 → ${e.message}`);
    // 定位：逐行找第一个可疑块
    const lines = code.split('\n');
    console.log('  行数', lines.length);
  }
}
process.exit(bad ? 1 : 0);
