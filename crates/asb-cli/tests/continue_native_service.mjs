import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, existsSync, writeFileSync } from 'node:fs';
import { once } from 'node:events';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { createServer } from 'node:net';
import { brokerRequest } from '../assets/scripts/continue-native-broker.mjs';
import { encodeSidecar } from '../assets/scripts/continue-native-sidecar.mjs';

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
  const digest = bytes => createHash('sha256').update(bytes).digest('hex');
  const archive = join(root, 'archive.tar'), sidecar = join(root, 'sidecar.jsonl');
  writeFileSync(archive, 'snapshot');
  writeFileSync(sidecar, encodeSidecar([{ordinal:0,request:{tool:'Read'},outcome:{ok:true,value:'replayed'}}], digest('fixture bundle'), digest('snapshot')));
  const markerPath = join(root, 'marker.sock');
  const marker = createServer(client => client.once('data', data => { assert.equal(data.toString(),'tool-end:continue-0\n'); client.end('1\n'); }));
  await new Promise(resolve => marker.listen(markerPath, resolve));
  const replaySocket = join(root, 'replay.sock');
  const replay = spawn(process.execPath, [service, replaySocket, program, sidecar, archive, markerPath], {stdio:['pipe','pipe','ignore']});
  const replayExit = once(replay,'exit');
  assert.equal((await once(replay.stdout,'data'))[0].toString(),'READY\n');
  assert.equal((await brokerRequest(replaySocket,{op:'invoke',request:{tool:'Read'}})).value,'replayed');
  replay.stdin.end('S\n');
  assert.equal((await replayExit)[0],0);
  await new Promise(resolve => marker.close(resolve));
  writeFileSync(archive,'different snapshot');
  const mismatch = spawn(process.execPath,[service,join(root,'bad.sock'),program,sidecar,archive,markerPath],{stdio:['ignore','ignore','ignore']});
  assert.notEqual((await once(mismatch,'exit'))[0],0);
  console.log('Continue recording service checks passed');
} finally { rmSync(root, { recursive: true, force: true }); }
