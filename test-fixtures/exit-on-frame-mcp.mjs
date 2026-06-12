// test-fixtures/exit-on-frame-mcp.mjs
// Spawns fine, then exits the moment it reads any input line — exercises
// the "backend spawns OK but its pump dies immediately" path. Such a
// backend must eventually hit the McpHost cooldown, not respawn forever.
import readline from 'node:readline';
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', () => process.exit(1));
