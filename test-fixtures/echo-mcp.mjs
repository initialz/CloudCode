// test-fixtures/echo-mcp.mjs
// Minimal MCP-over-stdio echo stub for browser-pipe testing (M1).
// Reads line-delimited JSON-RPC on stdin, writes responses on stdout.
import readline from 'node:readline';

const rl = readline.createInterface({ input: process.stdin });
function send(obj) { process.stdout.write(JSON.stringify(obj) + '\n'); }

rl.on('line', (line) => {
  line = line.trim();
  if (!line) return;
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.method === 'initialize') {
    send({ jsonrpc: '2.0', id: msg.id, result: {
      protocolVersion: '2024-11-05',
      capabilities: { tools: {} },
      serverInfo: { name: 'echo-mcp', version: '0.0.1' },
    }});
  } else if (msg.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: msg.id, result: { tools: [
      { name: 'echo', description: 'echo back text',
        inputSchema: { type: 'object', properties: { text: { type: 'string' } } } },
    ]}});
  } else if (msg.method === 'tools/call') {
    const text = msg.params?.arguments?.text ?? '';
    send({ jsonrpc: '2.0', id: msg.id, result: {
      content: [{ type: 'text', text: `echo: ${text}` }],
    }});
  } else if (msg.id !== undefined) {
    send({ jsonrpc: '2.0', id: msg.id, result: {} });
  }
});
