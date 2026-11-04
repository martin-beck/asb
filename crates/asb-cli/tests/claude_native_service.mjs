import assert from 'node:assert/strict';
import {mkdtemp, mkdir, writeFile, stat, rm} from 'node:fs/promises';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {brokerRequest} from '../assets/scripts/native-tool-broker.mjs';

const root = await mkdtemp(join(tmpdir(), 'asb-claude-service-'));
const service = new URL('../assets/scripts/native-tool-service.mjs', import.meta.url);
async function run(name, bad) {
  const dir = join(root, name), state = join(dir, 'state'), socket = join(dir, 'broker.sock');
  await mkdir(dir); await mkdir(state);
  if (bad) await writeFile(join(state, bad), 'unfinished');
  const program = join(dir, 'program'); await writeFile(program, 'fixture');
  const child = spawn(process.execPath, [service.pathname, socket, program, join(dir, 'sidecar')], {
    env: {...process.env, ASB_NATIVE_TOOL_AGENT: 'claude-code', ASB_NATIVE_HOOK_STATE: state},
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let stderr = '', ready = '';
  child.stderr.on('data', data => stderr += data);
  child.stdout.on('data', data => ready += data);
  while (!ready.includes('\n')) {
    const [data] = await once(child.stdout, 'data');
    if (!data.length) throw new Error('native service stopped before readiness');
  }
  assert.equal((await stat(socket)).mode & 0o777, 0o666);
  const {token} = await brokerRequest(socket, {op: 'start', request: {tool: 'fixture'}});
  await brokerRequest(socket, {op: 'finish', token, outcome: {ok: true, value: 'fixture'}});
  child.stdin.end('S\n');
  const [code] = await once(child, 'exit');
  return {code, stderr};
}
try {
  const clean = await run('clean', ''); assert.equal(clean.code, 0, clean.stderr);
  for (const name of ['hook-failed', 'pending.json']) {
    const result = await run(name.replace('.', '-'), name);
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, /missing or unfinished/);
  }
  console.log('Claude native service: clean and failed settlement passed');
} finally { await rm(root, {recursive: true, force: true}); }
