// cc-browser 诊断探针:绕开整个 cloudcode 管道,直接用和 cc-browser
// 一模一样的命令(headed + 持久 profile)驱动 playwright-mcp,逐帧带
// 时间戳,量"握手 / browser_navigate"在本机的真实耗时。
//
// 用法:
//   pkill -f "@playwright/mcp"; pkill -f "playwright-mcp"; sleep 1   # 先清场
//   node scripts/mcp-nav-probe.mjs                                   # 会弹浏览器,90s 自停
//
// 判读:
//   "-> browser_navigate" 到 "<- ...id 2..." 隔 ~10s → playwright-mcp 正常,
//        60s/卡死是 cloudcode 管道的锅。
//   隔 60s+ 或一直不回 / 有 ERR → 卡在本机 playwright-mcp / headed 启动。
//   "-> initialize" 后迟迟没 "<- ...id 0..." → 握手就慢。
import { spawn } from 'node:child_process';

const t0 = Date.now();
const el = () => ((Date.now() - t0) / 1000).toFixed(2) + 's';
const PROFILE = process.env.HOME
  + '/.local/state/cloudcode/browser-profile';
const args = [
  '-y', '@playwright/mcp@0.0.76',
  '--user-data-dir=' + PROFILE,
  '--output-dir=/tmp/cc-probe-out',
];
const child = spawn('npx', args,
  { stdio: ['pipe', 'pipe', 'pipe'] });

let buf = '';
child.stdout.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (line.trim()) {
      console.log('[' + el() + '] <- ' + line.slice(0, 160));
    }
  }
});
child.stderr.on('data', (d) => {
  process.stderr.write('[' + el() + '] ERR: '
    + String(d).slice(0, 200));
});
child.on('exit', (c, s) => {
  console.log('[' + el() + '] exit code=' + c + ' sig=' + s);
});

function send(o) {
  const tag = (o.params && o.params.name) || o.method;
  console.log('[' + el() + '] -> ' + tag);
  child.stdin.write(JSON.stringify(o) + '\n');
}

const init = {
  jsonrpc: '2.0', id: 0, method: 'initialize',
  params: {
    protocolVersion: '2025-06-18',
    capabilities: {},
    clientInfo: { name: 'p', version: '0' },
  },
};
const inited = {
  jsonrpc: '2.0', method: 'notifications/initialized',
};
const nav = {
  jsonrpc: '2.0', id: 2, method: 'tools/call',
  params: {
    name: 'browser_navigate',
    arguments: { url: 'https://www.baidu.com' },
  },
};

setTimeout(() => send(init), 500);
setTimeout(() => send(inited), 1500);
setTimeout(() => send(nav), 2000);
setTimeout(() => {
  console.log('[' + el() + '] killing');
  child.kill('SIGKILL');
  process.exit(0);
}, 90000);
