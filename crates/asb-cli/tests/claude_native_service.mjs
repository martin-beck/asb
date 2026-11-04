import assert from 'node:assert/strict';
import {mkdtemp, mkdir, readFile, writeFile, stat, rm} from 'node:fs/promises';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {brokerRequest} from '../assets/scripts/native-tool-broker.mjs';
import {encodeSidecar} from '../assets/scripts/native-tool-sidecar.mjs';
import {createHash} from 'node:crypto';
import {createServer} from 'node:net';
import http from 'node:http';
import tls from 'node:tls';

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
async function runReplay(name, plan, consume = false) {
  const dir = join(root, name), state = join(dir, 'state'), socket = join(dir, 'broker.sock');
  await mkdir(dir); await mkdir(state);
  const program = join(dir, 'program'), sidecar = join(dir, 'sidecar'), planFile = join(dir, 'plan.json'), readyFile = join(dir, 'ready.json'), markerPath = join(dir, 'marker.sock');
  const source = Buffer.from('fixture program'); await writeFile(program, source);
  const sha = createHash('sha256').update(source).digest('hex'), snapshot = 'a'.repeat(64);
  await writeFile(sidecar, encodeSidecar([{ordinal: 0, request: {tool: 'fixture'}, outcome: {ok: true, value: 'recorded'}}], sha, snapshot, 'claude-code'));
  await writeFile(planFile, JSON.stringify(plan));
  const marker = createServer(socket => { socket.once('data', () => socket.end('1\n')); });
  await new Promise(resolve => marker.listen(markerPath, resolve));
  const child = spawn(process.execPath, [service.pathname, socket, program, sidecar, `sha256:${snapshot}`, markerPath], {
    env: {...process.env, ASB_NATIVE_TOOL_AGENT: 'claude-code', ASB_NATIVE_HOOK_STATE: state, ASB_CLAUDE_WEB_PLAN: planFile, ASB_CLAUDE_WEB_READY: readyFile, ASB_CLAUDE_WEB_RUNTIME: dir},
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let stderr = '', ready = '';
  child.stderr.on('data', data => stderr += data); child.stdout.on('data', data => ready += data);
  while (!ready.includes('\n')) await once(child.stdout, 'data');
  assert.equal((await stat(readyFile)).mode & 0o777, 0o644);
  const endpoint = JSON.parse(await readFile(readyFile, 'utf8'));
  assert.match(endpoint.address, /^http:\/\/127\.0\.0\.1:/);
  const cert = await readFile(endpoint.certPath);
  assert.match(cert.toString(), /BEGIN CERTIFICATE/);
  if (consume) {
    const address = new URL(endpoint.address);
    const response = await new Promise((resolve, reject) => {
      const request = http.request({host: address.hostname, port: address.port, method: 'CONNECT', path: 'example.test:443'});
      request.on('connect', (_response, socket) => {
        const secure = tls.connect({socket, servername: 'example.test', ca: cert}, () => secure.write('GET /page HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n'));
        let text = ''; secure.setEncoding('utf8'); secure.on('data', chunk => text += chunk); secure.on('end', () => resolve(text)); secure.on('error', reject);
      }); request.on('error', reject); request.end();
    });
    assert.match(response, /fixture/);
  }
  assert.equal((await brokerRequest(socket, {op: 'invoke', request: {tool: 'fixture'}})).value, 'recorded');
  child.stdin.end('S\n');
  const [code] = await once(child, 'exit');
  await new Promise(resolve => marker.close(resolve));
  await assert.rejects(readFile(readyFile), {code: 'ENOENT'});
  await assert.rejects(readFile(endpoint.certPath), {code: 'ENOENT'});
  return {code, stderr};
}
try {
  assert.equal((await runReplay('replay-clean', [])).code, 0);
  assert.equal((await runReplay('replay-consumed', [{ordinal: 0, url: 'https://example.test/page', status: 200, page: 'fixture'}], true)).code, 0);
  const unfinished = await runReplay('replay-unfinished', [{ordinal: 0, url: 'https://example.test/page', status: 200, page: 'fixture'}]);
  assert.notEqual(unfinished.code, 0);
  assert.match(unfinished.stderr, /unconsumed/);
  const clean = await run('clean', ''); assert.equal(clean.code, 0, clean.stderr);
  for (const name of ['hook-failed', 'pending.json']) {
    const result = await run(name.replace('.', '-'), name);
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, /missing or unfinished/);
  }
  console.log('Claude native service: clean and failed settlement passed');
} finally { await rm(root, {recursive: true, force: true}); }
