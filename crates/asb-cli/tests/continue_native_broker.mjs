import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { ContinueBroker, brokerRequest } from '../assets/scripts/continue-native-broker.mjs';
import { createNativeDispatcher } from '../assets/scripts/continue-native-dispatch.mjs';

const root = mkdtempSync(join(tmpdir(), 'asb-continue-broker-'));
const socket = join(root, 'broker.sock');
const moduleUrl = new URL('../assets/scripts/continue-native-broker.mjs', import.meta.url).href;
function child(mode) {
  const code = `import {brokerRequest} from ${JSON.stringify(moduleUrl)};\nconst p=process.argv[1];\nif(process.argv[2]==='record'){const s=await brokerRequest(p,{op:'start',request:{tool:'Fetch',args:{url:'fixture'}}});await brokerRequest(p,{op:'finish',token:s.token,outcome:{ok:true,value:'fetch-result'}})}else{const r=await brokerRequest(p,{op:'invoke',request:{tool:'Fetch',args:{url:'fixture'}}});if(r.value!=='fetch-result')process.exit(3)}`;
  const process = spawn(processExec, ['--input-type=module', '-e', code, socket, mode], { stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  process.stderr.on('data', value => { stderr += value; });
  return new Promise((resolve, reject) => { process.once('error', reject); process.once('exit', code => code === 0 ? resolve() : reject(new Error(`child ${code}: ${stderr}`))); });
}
const processExec = process.execPath;
try {
  const record = new ContinueBroker('record');
  await record.listen(socket);
  const first = await brokerRequest(socket, { op: 'start', request: { tool: 'Read', args: { path: 'fixture' } } });
  await child('record');
  await brokerRequest(socket, { op: 'finish', token: first.token, outcome: { ok: true, value: 'read-result' } });
  const events = await record.finish();
  assert.deepEqual(events.map(event => event.ordinal), [1, 0]);
  const effects = [];
  const replay = new ContinueBroker('replay', events, async event => { effects.push(event.ordinal); });
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
  const cancelled = new ContinueBroker('replay', events);
  await cancelled.listen(socket);
  const waiting = assert.rejects(brokerRequest(socket, { op: 'invoke', request: { tool: 'Fetch', args: { url: 'fixture' } } }));
  await new Promise(resolve => setTimeout(resolve, 30));
  await cancelled.abort();
  await waiting;
  const unfinished = new ContinueBroker('record');
  await unfinished.listen(socket);
  await brokerRequest(socket, { op: 'start', request: { tool: 'Read' } });
  await assert.rejects(unfinished.finish(), /unfinished/);
  await unfinished.abort();
  const roundTrip = new ContinueBroker('record');
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
  const offline = new ContinueBroker('replay', nativeEvents);
  await offline.listen(socket);
  const replayDispatch = createNativeDispatcher('replay', socket, { isDelegation: call => call.name === 'Subagent' });
  assert.equal(await replayDispatch({ name: 'Fetch', arguments: { url: 'local' } }, {}, async () => { executions++; return 'live'; }), 'recorded');
  assert.equal(await replayDispatch({ name: 'Subagent', arguments: {} }, {}, async () => { executions++; return 'delegated'; }), 'delegated');
  await offline.finish();
  assert.equal(executions, 3, 'the replayed native call must never execute');
  console.log('Continue broker parent/child ordering checks passed');
} finally { rmSync(root, { recursive: true, force: true }); }
