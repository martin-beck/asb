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
  for (const thrown of [{ reason: 'fixture' }, 'cancelled', null, undefined, 4n]) {
    const throwing = new NativeToolBroker('record');
    await throwing.listen(socket);
    const dispatchThrow = createNativeDispatcher('record', socket);
    let caught = false;
    try { await dispatchThrow({ name: 'Fetch', arguments: {} }, {}, async () => { throw thrown; }); }
    catch (error) { caught = true; assert.equal(error, thrown); }
    assert.equal(caught, true);
    const thrownEvents = await throwing.finish();
    const playback = new NativeToolBroker('replay', thrownEvents);
    await playback.listen(socket);
    caught = false;
    try { await createNativeDispatcher('replay', socket)({ name: 'Fetch', arguments: {} }, {}, async () => assert.fail('must not execute')); }
    catch (error) { caught = true; assert.deepEqual(error, thrown); }
    assert.equal(caught, true);
    await playback.finish();
  }
  const invalid = new NativeToolBroker('record');
  await invalid.listen(socket);
  const invalidDispatch = createNativeDispatcher('record', socket);
  const thrown = Symbol('unrepresentable');
  await assert.rejects(invalidDispatch({ name: 'Fetch', arguments: {} }, {}, async () => { throw thrown; }), error => error === thrown);
  assert.equal(invalid.pending.size, 0);
  await assert.rejects(invalid.finish(), /unfinished/);
  await invalid.abort();
  for (const makeValue of [() => undefined, () => 1n, () => { const value = {}; value.self = value; return value; }]) {
    const invalidResult = new NativeToolBroker('record');
    await invalidResult.listen(socket);
    const dispatch = createNativeDispatcher('record', socket);
    await assert.rejects(dispatch({ name: 'Read', arguments: {} }, {}, async () => makeValue()), TypeError);
    assert.equal(invalidResult.pending.size, 0, 'unserializable results must release their token');
    await assert.rejects(invalidResult.finish(), /unfinished/);
    await invalidResult.abort();
  }
  const invalidEncoder = new NativeToolBroker('record');
  await invalidEncoder.listen(socket);
  const encoderError = new Error('fixture encoder failure');
  const encoderDispatch = createNativeDispatcher('record', socket, { encodeResult: () => { throw encoderError; } });
  await assert.rejects(encoderDispatch({ name: 'Read', arguments: {} }, {}, async () => 'value'), error => error === encoderError);
  assert.equal(invalidEncoder.pending.size, 0);
  await assert.rejects(invalidEncoder.finish(), /unfinished/);
  await invalidEncoder.abort();
  const cyclic = {}; cyclic.self = cyclic;
  await assert.rejects(brokerRequest(socket, cyclic), TypeError);
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
  const delayedEffects = [];
  const delayed = new NativeToolBroker('replay', [
    {ordinal: 1, request: {tool: 'child'}, outcome: {ok: true, value: 'child'}},
    {ordinal: 0, request: {tool: 'parent'}, outcome: {ok: true, value: 'parent'}},
  ], async event => { delayedEffects.push(event.ordinal); });
  await delayed.listen(socket);
  const parent = await brokerRequest(socket, {op: 'reserve', request: {tool: 'parent'}});
  const childCall = await brokerRequest(socket, {op: 'reserve', request: {tool: 'child'}});
  await assert.rejects(delayed.finish(), /unfinished/);
  assert.deepEqual(delayedEffects, []);
  assert.deepEqual((await brokerRequest(socket, {op: 'inspect', token: childCall.token})).outcome, {ok: true, value: 'child'});
  assert.deepEqual(delayedEffects, []);
  assert.equal((await brokerRequest(socket, {op: 'release', token: childCall.token})).value, 'child');
  assert.deepEqual(delayedEffects, [1]);
  assert.equal((await brokerRequest(socket, {op: 'release', token: parent.token})).value, 'parent');
  assert.deepEqual(delayedEffects, [1, 0]);
  await assert.rejects(brokerRequest(socket, {op: 'release', token: parent.token}), /no active/);
  await delayed.finish();
  const duplicateBroker = new NativeToolBroker('replay', [
    {ordinal: 0, request: {tool: 'first'}, outcome: {ok: true, value: 'first'}},
    {ordinal: 1, request: {tool: 'later'}, outcome: {ok: true, value: 'later'}},
  ]);
  await duplicateBroker.listen(socket);
  const lateCall = await brokerRequest(socket, {op: 'reserve', request: {tool: 'later'}});
  await assert.rejects(brokerRequest(socket, {op: 'reserve', request: {tool: 'later'}}), /no matching/);
  const firstCall = await brokerRequest(socket, {op: 'reserve', request: {tool: 'first'}});
  const lateOutcome = brokerRequest(socket, {op: 'release', token: lateCall.token});
  assert.equal((await brokerRequest(socket, {op: 'release', token: firstCall.token})).value, 'first');
  assert.equal((await lateOutcome).value, 'later');
  await duplicateBroker.finish();
  // Broker messages may arrive in a different order than the snapshot
  // service completed them. Preserve the snapshot order for replay.
  const orderedRecord = new NativeToolBroker('record');
  await orderedRecord.listen(socket);
  const earlier = await brokerRequest(socket, {op:'start', request:{tool:'earlier'}});
  const later = await brokerRequest(socket, {op:'start', request:{tool:'later'}});
  await brokerRequest(socket, {op:'finish', token:later.token, snapshot_sequence:3, outcome:{ok:true,value:'later'}});
  await brokerRequest(socket, {op:'finish', token:earlier.token, snapshot_sequence:2, outcome:{ok:true,value:'earlier'}});
  const orderedEvents = JSON.parse(await orderedRecord.finish(events => JSON.stringify(events)));
  assert.deepEqual(orderedEvents.map(event => event.ordinal), [0,1]);
  const orderedReplay = new NativeToolBroker('replay', orderedEvents);
  await orderedReplay.listen(socket);
  assert.equal((await brokerRequest(socket, {op:'invoke', request:{tool:'earlier'}})).value, 'earlier');
  assert.equal((await brokerRequest(socket, {op:'invoke', request:{tool:'later'}})).value, 'later');
  await orderedReplay.finish();
  const partialOrder = new NativeToolBroker('record');
  await partialOrder.listen(socket);
  const withSequence = await brokerRequest(socket, {op:'start', request:{tool:'one'}});
  const withoutSequence = await brokerRequest(socket, {op:'start', request:{tool:'two'}});
  await brokerRequest(socket, {op:'finish', token:withSequence.token, snapshot_sequence:2, outcome:{ok:true,value:1}});
  await brokerRequest(socket, {op:'finish', token:withoutSequence.token, outcome:{ok:true,value:2}});
  await assert.rejects(partialOrder.finish(), /snapshot completion order is incomplete/);
  await partialOrder.abort();
  const duplicateSequence = new NativeToolBroker('record');
  await duplicateSequence.listen(socket);
  const firstSequence = await brokerRequest(socket, {op:'start', request:{tool:'one'}});
  const secondSequence = await brokerRequest(socket, {op:'start', request:{tool:'two'}});
  await brokerRequest(socket, {op:'finish', token:firstSequence.token, snapshot_sequence:2, outcome:{ok:true,value:1}});
  await assert.rejects(brokerRequest(socket, {op:'finish', token:secondSequence.token, snapshot_sequence:2, outcome:{ok:true,value:2}}), /snapshot completion sequence is invalid/);
  await assert.rejects(duplicateSequence.finish(), /unfinished/);
  await duplicateSequence.abort();
  const gapOrder = new NativeToolBroker('record');
  await gapOrder.listen(socket);
  const gap = await brokerRequest(socket, {op:'start', request:{tool:'one'}});
  await brokerRequest(socket, {op:'finish', token:gap.token, snapshot_sequence:3, outcome:{ok:true,value:1}});
  await assert.rejects(gapOrder.finish(), /snapshot completion order is incomplete/);
  await gapOrder.abort();
  await new Promise(resolve => marker.close(resolve));
  await assert.rejects(snapshotBoundary(markerSocket,'record','begin',0), /unavailable/);
  console.log('Continue broker parent/child ordering checks passed');
} finally { rmSync(root, { recursive: true, force: true }); }
