import assert from 'node:assert/strict';
import {mkdtemp, rm} from 'node:fs/promises';
import {createServer} from 'node:net';
import os from 'node:os';
import path from 'node:path';
import {NativeToolBroker} from '../assets/scripts/native-tool-broker.mjs';
import {snapshotBoundary} from '../assets/scripts/native-snapshot-boundary.mjs';
import {createClineNativeHooks} from '../assets/scripts/cline-native-hooks.mjs';
import {createClineNativeTransport} from '../assets/scripts/cline-native-transport.mjs';

const root = await mkdtemp(path.join(os.tmpdir(), 'cline-tape-'));
const socket = path.join(root, 'broker.sock');
const marker = path.join(root, 'marker.sock');
const boundaries = [];
const server = createServer(connection => {
  let text = '';
  connection.on('data', data => {
    text += data.toString();
    if (text.includes('\n')) { boundaries.push(text.trim()); connection.end('1\n'); }
  });
});
const context = (agent, id, name, input, result) => ({
  snapshot: {agentId: agent}, toolCall: {toolCallId: id}, tool: {name}, input, result,
});
const parent = context('root', 'same', 'spawn_agent', {task: 'fixture'}, {output: 'done'});
const child = context('child', 'same', 'fetch_web_content', {url: 'https://example.test'}, {output: 'recorded'});
try {
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(marker, resolve); });
  const recording = new NativeToolBroker('record');
  await recording.listen(socket);
  const record = createClineNativeHooks('record', createClineNativeTransport(socket, marker));
  await record.hooks.beforeTool(parent);
  await record.hooks.beforeTool(child);
  await record.hooks.afterTool(child);
  await record.hooks.afterTool(parent);
  record.assertSettled();
  const events = await recording.finish();
  assert.equal(events.length, 2);
  assert.deepEqual(boundaries.map(line => line.split(':').slice(0, 2).join(':')), [
    'tool-begin:cline-0', 'tool-begin:cline-1', 'tool-end:cline-1', 'tool-end:cline-0',
  ]);

  const playback = new NativeToolBroker('replay', events, event => snapshotBoundary(marker, 'replay', 'end', event.ordinal, 'cline'));
  await playback.listen(socket);
  const replay = createClineNativeHooks('replay', createClineNativeTransport(socket, marker));
  assert.equal(await replay.hooks.beforeTool(parent), undefined, 'delegate must execute');
  assert.equal((await replay.hooks.beforeTool(child)).skip, true, 'web request must not execute');
  assert.deepEqual((await replay.hooks.afterTool({...child, result: {isError: true}})).result, {output: 'recorded'});
  await replay.hooks.afterTool(parent);
  replay.assertSettled();
  await playback.finish();
  assert.deepEqual(boundaries.slice(4), ['tool-end:cline-1', 'tool-end:cline-0']);

  const mismatchBroker = new NativeToolBroker('replay', events);
  await mismatchBroker.listen(socket);
  const mismatch = createClineNativeHooks('replay', createClineNativeTransport(socket, marker));
  await assert.rejects(mismatch.hooks.beforeTool({...child, input: {url: 'https://different.test'}}), /no matching remaining tool request/);
  assert.throws(() => mismatch.assertSettled(), /capture or replay failed/);
  await mismatchBroker.abort();
} finally {
  await new Promise(resolve => server.close(resolve));
  await rm(root, {recursive: true, force: true});
}
console.log('Cline native transport: nested snapshots and offline broker replay passed');
