import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { connect, createServer } from 'node:net';
import { once } from 'node:events';
import { NativeToolBroker, brokerRequest } from '../assets/scripts/native-tool-broker.mjs';
import { createNativeDispatcher } from '../assets/scripts/continue-native-dispatch.mjs';
import { sealBroker, sealPendingBroker, replayBroker } from '../assets/scripts/native-tool-custody.mjs';
import { createHash } from 'node:crypto';
import { recordBoundaries, replayEffects, snapshotBoundary } from '../assets/scripts/native-snapshot-boundary.mjs';

const root = mkdtempSync(join(tmpdir(), 'asb-continue-broker-'));
const socket = join(root, 'broker.sock');
const moduleUrl = new URL('../assets/scripts/native-tool-broker.mjs', import.meta.url).href;
function child(mode) {
  const code = `import {brokerRequest} from ${JSON.stringify(moduleUrl)};\nconst p=process.argv[1];\nif(process.argv[2]==='record'){const s=await brokerRequest(p,{op:'start',request:{tool:'Fetch',args:{url:'fixture'}}});await brokerRequest(p,{op:'finish',token:s.token,outcome:{ok:true,value:'fetch-result'}})}else{const r=await brokerRequest(p,{op:'invoke',request:{tool:'Fetch',args:{url:'fixture'}}});if(r.value!=='fetch-result')process.exit(3)}`;
  const process = spawn(processExec, ['--input-type=module', '-e', code, socket, mode], { stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  process.stderr.on('data', value => { stderr += value; });
  return new Promise((resolve, reject) => { process.once('error', reject); process.once('exit', code => code === 0 ? resolve() : reject(new Error(`child ${code}: ${stderr}`))); });
}
const processExec = process.execPath;
try {
  const record = new NativeToolBroker('record');
  await record.listen(socket);
  const first = await brokerRequest(socket, { op: 'start', request: { tool: 'Read', args: { path: 'fixture' } } });
  await child('record');
  await brokerRequest(socket, { op: 'finish', token: first.token, outcome: { ok: true, value: 'read-result' } });
  const events = await record.finish();
  assert.deepEqual(events.map(event => event.ordinal), [1, 0]);
  const effects = [];
  const replay = new NativeToolBroker('replay', events, async event => { effects.push(event.ordinal); });
  await replay.listen(socket);
  const second = child('replay');
  // Let the child request the later dispatch first; the broker must wait for
  // its recorded predecessor while preserving recorded completion order.
  await new Promise(resolve => setTimeout(resolve, 100));
  const firstReplay = brokerRequest(socket, { op: 'invoke', request: { tool: 'Read', args: { path: 'fixture' } } });
  await second;
  assert.equal((await firstReplay).value, 'read-result');
  await replay.finish();
  assert.deepEqual(effects, [1, 0]);
  const cancelled = new NativeToolBroker('replay', events);
  await cancelled.listen(socket);
  const waiting = assert.rejects(brokerRequest(socket, { op: 'invoke', request: { tool: 'Fetch', args: { url: 'fixture' } } }));
  await new Promise(resolve => setTimeout(resolve, 30));
  await cancelled.abort();
  await waiting;
  const unfinished = new NativeToolBroker('record');
  await unfinished.listen(socket);
  await brokerRequest(socket, { op: 'start', request: { tool: 'Read' } });
  await assert.rejects(unfinished.finish(), /unfinished/);
  await unfinished.abort();
  const roundTrip = new NativeToolBroker('record');
  await roundTrip.listen(socket);
  const boundaries = [];
  const recordDispatch = createNativeDispatcher('record', socket, {
    isDelegation: call => call.name === 'Subagent',
    begin: async () => boundaries.push('begin'),
    end: async () => boundaries.push('end'),
  });
  let executions = 0;
  assert.equal(await recordDispatch({ name: 'Fetch', arguments: { url: 'local' } }, {}, async () => { boundaries.push('tool'); executions++; return 'recorded'; }), 'recorded');
  assert.equal(await recordDispatch({ name: 'Subagent', arguments: {} }, {}, async () => { executions++; return 'delegated'; }), 'delegated');
  const nativeEvents = await roundTrip.finish();
  assert.equal(nativeEvents.length, 1);
  assert.deepEqual(boundaries, ['begin', 'tool', 'end']);
  const offline = new NativeToolBroker('replay', nativeEvents);
  await offline.listen(socket);
  const replayDispatch = createNativeDispatcher('replay', socket, { isDelegation: call => call.name === 'Subagent' });
  assert.equal(await replayDispatch({ name: 'Fetch', arguments: { url: 'local' } }, {}, async () => { executions++; return 'live'; }), 'recorded');
  assert.equal(await replayDispatch({ name: 'Subagent', arguments: {} }, {}, async () => { executions++; return 'delegated'; }), 'delegated');
  await offline.finish();
  assert.equal(executions, 3, 'the replayed native call must never execute');
  const invalid = new NativeToolBroker('record');
  await invalid.listen(socket);
  const invalidDispatch = createNativeDispatcher('record', socket);
  const thrown = { reason: 'non-Error value' };
  await assert.rejects(invalidDispatch({ name: 'Fetch', arguments: {} }, {}, async () => { throw thrown; }), error => error === thrown);
  assert.equal(invalid.pending.size, 0, 'invalid exchanges must not remain pending');
  await assert.rejects(invalid.finish(), /unfinished/);
  await invalid.abort();
  for (const failingBoundary of ['begin', 'end']) {
    const incomplete = new NativeToolBroker('record');
    await incomplete.listen(socket);
    const boundaryError = new Error(`${failingBoundary} failed`);
    let calls = 0;
    const boundaryDispatch = createNativeDispatcher('record', socket, {
      [failingBoundary]: async () => { throw boundaryError; },
    });
    await assert.rejects(boundaryDispatch({ name: 'Write', arguments: {} }, {}, async () => { calls++; return 'ok'; }), error => error === boundaryError);
    assert.equal(calls, failingBoundary === 'begin' ? 0 : 1);
    assert.equal(incomplete.pending.size, 0);
    await assert.rejects(incomplete.finish(), /unfinished/);
    await incomplete.abort();
  }
  const sealing = new NativeToolBroker('record');
  await sealing.listen(socket);
  const late = connect(socket);
  await once(late, 'connect');
  // Let the server accept the connection, but withhold the request until
  // sealing begins. It must never enter the already snapshotted session.
  while (!sealing.connections.size) await new Promise(resolve => setImmediate(resolve));
  const sealResult = sealing.finish(events => JSON.stringify(events));
  late.write(JSON.stringify({ op: 'start', request: { tool: 'Write' } }) + '\n');
  const [reply] = await once(late, 'data');
  assert.match(reply.toString(), /sealing/);
  assert.equal(await sealResult, '[]');
  const custody = new NativeToolBroker('record');
  await custody.listen(socket);
  const dispatch = createNativeDispatcher('record', socket);
  assert.equal(await dispatch({name:'Fetch',arguments:{url:'fixture'}},{},async()=> 'captured'), 'captured');
  const programSha256 = createHash('sha256').update('fixture-program').digest('hex');
  const workspaceEffectSha256 = createHash('sha256').update('fixture-effects').digest('hex');
  await assert.rejects(sealBroker(custody,{programSha256,workspaceEffectSha256:'invalid'}),/requires actual/);
  const sealed = await sealBroker(custody,{programSha256,workspaceEffectSha256});
  assert.throws(()=>replayBroker(sealed,{programSha256,workspaceEffectSha256:'0'.repeat(64),applyEffects:async()=>{}}),/does not match/);
  let applied = 0;
  const restored = replayBroker(sealed,{programSha256,workspaceEffectSha256,applyEffects:async()=>{ applied++; }});
  await restored.listen(socket);
  const restoredDispatch = createNativeDispatcher('replay', socket);
  assert.equal(await restoredDispatch({name:'Fetch',arguments:{url:'fixture'}},{},async()=>{throw new Error('must not execute')}), 'captured');
  await restored.finish();
  assert.equal(applied,1);
  const pendingBroker = new NativeToolBroker('record');
  await pendingBroker.listen(socket);
  await createNativeDispatcher('record',socket)({name:'Read',arguments:{}},{},async()=> 'ok');
  await assert.rejects(sealPendingBroker(pendingBroker,{programSha256:'invalid'}),/program digest/);
  const pendingSidecar = await sealPendingBroker(pendingBroker,{programSha256});
  assert.doesNotMatch(pendingSidecar.toString(), /workspace_effect/);
  assert.throws(()=>replayBroker(pendingSidecar,{programSha256,workspaceEffectSha256,applyEffects:async()=>{}}), /count|lifecycle/);
  // Exercise the real adapter marker protocol with a local fixture service.
  // This models effect application; it is not a Btrfs snapshot qualification.
  const markerSocket = join(root, 'marker.sock');
  const workspaceFile = join(root, 'workspace.txt');
  const markers = [];
  let replaying = false;
  const marker = createServer(client => {
    let data = '';
    client.on('data', chunk => {
      data += chunk;
      if (!data.includes('\n')) return;
      markers.push(data.trim());
      if (replaying) writeFileSync(workspaceFile, 'after');
      client.end('1\n');
    });
  });
  await new Promise(resolve => marker.listen(markerSocket, resolve));
  writeFileSync(workspaceFile, 'before');
  const filesystemRecord = new NativeToolBroker('record');
  await filesystemRecord.listen(socket);
  const writeRecord = createNativeDispatcher('record', socket, recordBoundaries(markerSocket));
  await writeRecord({name:'Write',arguments:{path:'workspace.txt'}},{},async()=>{writeFileSync(workspaceFile,'after'); return 'ok';});
  const filesystemEvents = await filesystemRecord.finish();
  assert.equal(readFileSync(workspaceFile,'utf8'),'after');
  writeFileSync(workspaceFile, 'before');
  replaying = true;
  const filesystemReplay = new NativeToolBroker('replay', filesystemEvents, replayEffects(markerSocket));
  await filesystemReplay.listen(socket);
  const writeReplay = createNativeDispatcher('replay', socket);
  assert.equal(await writeReplay({name:'Write',arguments:{path:'workspace.txt'}},{},async()=>{throw new Error('live write must not run')}),'ok');
  assert.equal(readFileSync(workspaceFile,'utf8'),'after');
  await filesystemReplay.finish();
  assert.match(markers[0], /^tool-begin:continue-0:\d+:\d+$/);
  assert.match(markers[1], /^tool-end:continue-0:\d+:\d+$/);
  assert.equal(markers[2], 'tool-end:continue-0');
  await new Promise(resolve => marker.close(resolve));
  await assert.rejects(snapshotBoundary(markerSocket,'record','begin',0), /unavailable/);
  console.log('Continue broker parent/child ordering checks passed');
} finally { rmSync(root, { recursive: true, force: true }); }
