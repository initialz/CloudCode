// test-fixtures/chatty-init-mcp.mjs
// Like echo-mcp but emits 12 server notifications BEFORE the initialize
// response, to exercise the handshake-replay drain-and-swallow path
// (regression guard for the 0..10 swallow-cap bug).
import readline from 'node:readline';
const rl = readline.createInterface({ input: process.stdin });
function send(obj) { process.stdout.write(JSON.stringify(obj) + '\n'); }
rl.on('line', (line) => {
  line = line.trim();
  if (!line) return;
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.method === 'initialize') {
    for (let i = 0; i < 12; i++) {
      send({ jsonrpc: '2.0', method: 'notifications/progress', params: { n: i } });
    }
    send({ jsonrpc: '2.0', id: msg.id, result: {
      protocolVersion: '2024-11-05', capabilities: { tools: {} },
      serverInfo: { name: 'chatty-mcp', version: '0.0.1' },
    }});
  } else if (msg.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: msg.id, result: { tools: [
      { name: 'echo', description: 'echo', inputSchema: { type: 'object' } },
    ]}});
  } else if (msg.id !== undefined) {
    send({ jsonrpc: '2.0', id: msg.id, result: {} });
  }
});
