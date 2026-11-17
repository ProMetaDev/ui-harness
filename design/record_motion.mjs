// 录一段原型的真实动效：Chrome headless + CDP 连续截图 → ffmpeg 合成 GIF。
// 这不是产品代码，只是设计评审用的素材工具。
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';

const CHROME = 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe';
const PORT = 9333;
const URL = process.argv[2] || 'file:///X:/dsh_ws/harness-v2/design/ui-concept.html?clean=1';
const FRAMES = path.resolve(process.argv[3] || 'X:/dsh_ws/harness-v2/out/motion-frames');
const COUNT = Number(process.argv[4] || 60);
const INTERVAL = Number(process.argv[5] || 100);

fs.rmSync(FRAMES, { recursive: true, force: true });
fs.mkdirSync(FRAMES, { recursive: true });

const profile = path.join(FRAMES, '_profile');
const chrome = spawn(
  CHROME,
  [
    '--headless=new',
    '--disable-gpu',
    '--hide-scrollbars',
    '--no-first-run',
    '--remote-allow-origins=*',
    `--remote-debugging-port=${PORT}`,
    `--user-data-dir=${profile}`,
    '--window-size=1440,900',
    URL,
  ],
  { stdio: 'ignore' },
);

async function version() {
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${PORT}/json/version`);
      if (r.ok) return r.json();
    } catch {}
    await sleep(100);
  }
  throw new Error('Chrome devtools not ready');
}

function connect(wsUrl) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl);
    const pending = new Map();
    let id = 0;
    ws.onopen = () => resolve({
      call(method, params = {}) {
        return new Promise((res, rej) => {
          const n = ++id;
          pending.set(n, { res, rej });
          ws.send(JSON.stringify({ id: n, method, params }));
        });
      },
      close() { ws.close(); },
    });
    ws.onerror = reject;
    ws.onmessage = (e) => {
      const m = JSON.parse(e.data);
      if (!m.id || !pending.has(m.id)) return;
      const p = pending.get(m.id);
      pending.delete(m.id);
      if (m.error) p.rej(new Error(`${m.error.message} (method pending)`));
      else p.res(m.result);
    };
  });
}

try {
  await version();
  const pages = await fetch(`http://127.0.0.1:${PORT}/json/list`).then((r) => r.json());
  const target = pages.find((p) => p.type === 'page');
  if (!target) throw new Error('no page target');
  const cdp = await connect(target.webSocketDebuggerUrl);
  await cdp.call('Page.enable');
  await cdp.call('Emulation.setDeviceMetricsOverride', {
    width: 1440,
    height: 900,
    deviceScaleFactor: 1,
    mobile: false,
  });
  // 有些 Windows/无头环境会把“减少动态效果”报成 reduce；录屏时强制 no-preference
  await cdp.call('Emulation.setEmulatedMedia', {
    features: [{ name: 'prefers-reduced-motion', value: 'no-preference' }],
  });
  // 重新导航一次：确保入场动画从 0 开始（否则 Chrome 启动时已经放完了）
  const bust = URL + (URL.includes('?') ? '&' : '?') + 'rec=' + Date.now();
  await cdp.call('Page.navigate', { url: bust });
  await sleep(600);
  const probe = await cdp.call('Runtime.evaluate', {
    expression:
      "JSON.stringify({reduce:matchMedia('(prefers-reduced-motion: reduce)').matches," +
      "anim:getComputedStyle(document.querySelector('.entry')||document.body).animationName," +
      "stage:(document.querySelector('.app')||{}).className," +
      "motion:document.documentElement.dataset.motion})",
  });
  console.log('PROBE', probe.result && probe.result.value);

  // 第 0 帧可能还是空白，让 ffmpeg 从第 2 帧开始用
  for (let i = 0; i < COUNT; i++) {
    // 第 8 帧按一次“重播”，让入场动画在录屏中真正出现
    if (i === 8) {
      await cdp.call('Runtime.evaluate', { expression: 'window.__uiReplay && window.__uiReplay()' });
    }
    const shot = await cdp.call('Page.captureScreenshot', {
      format: 'png',
      fromSurface: true,
      captureBeyondViewport: false,
    });
    fs.writeFileSync(path.join(FRAMES, `frame-${String(i).padStart(3, '0')}.png`), Buffer.from(shot.data, 'base64'));
    await sleep(INTERVAL);
  }
  cdp.close();
} finally {
  try {
    spawnSync('taskkill', ['/PID', String(chrome.pid), '/T', '/F'], { stdio: 'ignore' });
  } catch {}
}

python();

function python() {
  const ff = 'X:\\dsh_ws\\tools\\ffmpeg.exe';
  const out = 'X:\\dsh_ws\\harness-v2\\design\\motion.gif';
  const args = [
    '-y', '-framerate', '10', '-start_number', '8',
    '-i', path.join(FRAMES, 'frame-%03d.png'),
    '-vf', 'scale=960:-1:flags=lanczos,split[s0][s1];[s0]palettegen=max_colors=128[p];[s1][p]paletteuse=dither=bayer',
    '-loop', '0', out,
  ];
  const r = spawnSync(ff, args, { stdio: 'ignore' });
  if (r.status === 0) console.log(`saved ${out}`);
  else console.log('ffmpeg failed');
}
