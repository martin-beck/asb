import assert from 'node:assert/strict';
import {mkdtemp, readdir, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createServer} from 'node:net';
import {spawn, spawnSync} from 'node:child_process';
import {NativeToolBroker} from '../assets/scripts/native-tool-broker.mjs';
import {recordHook, replayHook, settlePending} from '../assets/scripts/codex-native-hook.mjs';
import {encodePendingSidecar} from '../assets/scripts/native-tool-sidecar.mjs';

const directory = await mkdtemp(join(tmpdir(), 'asb-codex-broker-'));
const socket = join(directory, 'broker.sock'), marker = join(directory, 'marker.sock');
let sequence = 1;
const markers = [];
const server = createServer(client => {
  let line = '';
  client.on('data', part => {
    line += part;
    if (line.includes('\n')) {
      markers.push(line.trim());
      client.end(`${line.startsWith('tool-end:') ? ++sequence : sequence}\n`);
    }
  });
});
await new Promise(resolve => server.listen(marker, resolve));
const broker = new NativeToolBroker('record');
await broker.listen(socket);
const options = {directory, socket, marker};
const call = (session_id, command) => ({session_id, tool_use_id: 'same-id', tool_name:'Bash', tool_input:{command}});
try {
  const first = call('agent-a', 'first');
  const second = call('agent-b', 'second');
  await recordHook({...first, hook_event_name:'PreToolUse'}, options);
  await recordHook({...second, hook_event_name:'PreToolUse'}, options);
  await recordHook({...second, hook_event_name:'PostToolUse', tool_response:'two'}, options);
  await recordHook({...first, hook_event_name:'PostToolUse', tool_response:'one'}, options);
  const events = await broker.finish();
  if (process.env.ASB_CODEX_PENDING_FIXTURE) await writeFile(process.env.ASB_CODEX_PENDING_FIXTURE, encodePendingSidecar(events, '0'.repeat(64), 'codex'));
  assert.equal(events.length, 2);
  assert.deepEqual(events.map(event => event.outcome.value), [
    {session_id:'agent-b',tool_use_id:'same-id'}, {session_id:'agent-a',tool_use_id:'same-id'},
  ]);
  assert.deepEqual(events.map(event => event.snapshot_sequence), [2,3]);
  assert.equal(markers.length, 4);
  assert.equal((await readdir(directory)).filter(name => name.endsWith('.json')).length, 0);
  await assert.rejects(recordHook({...first, session_id:'', hook_event_name:'PreToolUse'}, options), /identity/);
  const effects = [];
  const recorded = 'Chunk ID: original\nWall time: 0.12 seconds\nProcess exited with code 7\nOutput:\nrecorded output';
  const replay = new NativeToolBroker('replay', [{ordinal:0, request:{tool:'Bash',input:{command:'must-not-run'}}, outcome:{ok:true,value:recorded}}], async event => effects.push(event.ordinal));
  await replay.listen(socket);
  const setup = await replayHook({session_id:'fresh-session',tool_use_id:'fresh-call',tool_name:'Bash',tool_input:{command:'must-not-run'},hook_event_name:'PreToolUse'}, {socket});
  assert.deepEqual(effects, [0]);
  const emitted = spawnSync('/bin/sh', ['-c', setup.hookSpecificOutput.updatedInput.command], {encoding:'utf8'});
  assert.equal(emitted.status, 7);
  assert.equal(emitted.stdout, 'recorded output');
  assert.equal(emitted.stderr, '');
  const normalized = 'Chunk ID: <asb-chunk>\nWall time: <asb-wall-time> seconds\nProcess exited with code 7\nOriginal token count: <asb-token-count>\nOutput:\nnormalized output';
  const normalizedEmit = spawnSync(process.execPath, [new URL('../assets/scripts/codex-native-hook.mjs', import.meta.url).pathname, 'emit', Buffer.from(normalized).toString('base64')], {encoding:'utf8'});
  assert.equal(normalizedEmit.status, 7);
  assert.equal(normalizedEmit.stdout, 'normalized output');
  await replay.finish();
  const runningEffects = [];
  const runningOutput = 'Chunk ID: <asb-chunk>\nWall time: <asb-wall-time> seconds\nProcess running with session ID <asb-session>\nOriginal token count: <asb-token-count>\nOutput:\npartial';
  const running = new NativeToolBroker('replay', [{ordinal:0,request:{tool:'Bash',input:{command:'background'}},outcome:{ok:true,value:{output:runningOutput,running_wait_ms:150}}}], async event => runningEffects.push(event.ordinal));
  await running.listen(socket);
  const runningSetup = await replayHook({session_id:'replay',tool_use_id:'running',tool_name:'Bash',tool_input:{command:'background'},hook_event_name:'PreToolUse'}, {socket});
  const emitter = spawn('/bin/sh',['-c',runningSetup.hookSpecificOutput.updatedInput.command],{stdio:['ignore','pipe','pipe']});
  let partial=''; emitter.stdout.on('data',chunk=>partial+=chunk);
  const runningCode = await new Promise((resolve,reject)=>{emitter.on('error',reject);emitter.on('close',resolve);});
  assert.equal(runningCode,0); assert.equal(partial,'partial');
  await settlePending({mode:'replay',broker:running});
  assert.deepEqual(runningEffects,[0]);
  await running.finish();
  sequence = 1;
  const wrapped = new NativeToolBroker('record');
  await wrapped.listen(socket);
  const sync = call('sync', 'printf wrapped; exit 7');
  const prepared = await recordHook({...sync, hook_event_name:'PreToolUse'}, options);
  const child = spawn('/bin/sh', ['-c', prepared.hookSpecificOutput.updatedInput.command], {env:{...process.env,ASB_CODEX_NATIVE_DIRECTORY:directory,ASB_CODEX_NATIVE_SOCKET:socket,ASB_CODEX_NATIVE_MARKER:marker},stdio:['ignore','pipe','pipe']});
  let stdout = ''; child.stdout.on('data', part => stdout += part);
  const code = await new Promise((resolve,reject) => {child.on('error',reject);child.on('close',resolve);});
  assert.equal(code,7); assert.equal(stdout,'wrapped');
  assert.equal((await readdir(directory)).filter(name => name.endsWith('.json')).length,0);
  await recordHook({...call('terminated-background','sleep 60'),hook_event_name:'PreToolUse'}, options);
  await settlePending({...options,mode:'record',broker:wrapped});
  assert.equal((await wrapped.finish()).length,2);
} finally {
  await new Promise(resolve => server.close(resolve));
  await rm(directory, {recursive:true,force:true});
}
