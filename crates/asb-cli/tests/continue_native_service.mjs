import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, existsSync, writeFileSync } from 'node:fs';
import { once } from 'node:events';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { brokerRequest } from '../assets/scripts/continue-native-broker.mjs';

const root = mkdtempSync(join(tmpdir(), 'asb-continue-service-'));
const service = fileURLToPath(new URL('../assets/scripts/continue-native-service.mjs', import.meta.url));
try {
  const program = join(root, 'bundle.mjs');
  writeFileSync(program, 'fixture bundle');
  for (const settled of [true, false]) {
    const socket = join(root, `${settled}.sock`), destination = join(root, `${settled}.jsonl`);
    const child = spawn(process.execPath, [service, socket, program, destination], { stdio: ['pipe', 'pipe', 'ignore'] });
    const exited = once(child, 'exit');
    assert.equal((await once(child.stdout, 'data'))[0].toString(), 'READY\n');
    const { token } = await brokerRequest(socket, { op: 'start', request: { tool: 'Read' } });
    await brokerRequest(socket, { op: 'finish', token, outcome: { ok: true, value: 'fixture' } });
    child.stdin.end(settled ? 'S\n' : '');
    const [code] = await exited;
    assert.equal(code, settled ? 0 : 1);
    assert.equal(existsSync(destination), settled);
    if (settled) {
      const bytes = readFileSync(destination, 'utf8');
      assert.match(bytes, /"event":"launch"/);
      assert.doesNotMatch(bytes, /workspace_effect/);
    }
  }
  console.log('Continue recording service checks passed');
} finally { rmSync(root, { recursive: true, force: true }); }
