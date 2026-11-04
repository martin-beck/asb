import assert from 'node:assert/strict';
import {createClineNativeHooks} from '../assets/scripts/cline-native-hooks.mjs';

const context = (agent, id, tool, input, result) => ({
  snapshot: {agentId: agent}, toolCall: {toolCallId: id}, tool: {name: tool}, input, result,
});
const parent = context('root', 'one', 'spawn_agent', {task: 'fixture'}, {output: 'done'});
const child = context('child', 'one', 'read_files', {files: ['fixture']}, {output: 'contents'});

const events = [];
const transport = {
  async start(request) { events.push(['start', request.tool]); return events.length; },
  async begin(request) { events.push(['begin', request.tool]); },
  async end(token) { events.push(['end', token]); },
  async finish(token, result) { events.push(['finish', token, result]); },
};
const record = createClineNativeHooks('record', transport);
await record.hooks.beforeTool(parent);
await record.hooks.beforeTool(child);
await record.hooks.afterTool(child);
await record.hooks.afterTool(parent);
record.assertSettled();
assert.deepEqual(events.map(event => event[0]), ['start', 'begin', 'start', 'begin', 'end', 'finish', 'end', 'finish']);
assert.equal(events[5][2].json, JSON.stringify(child.result));

let finishDelegate;
const parentResult = new Promise(resolve => { finishDelegate = resolve; });
const replay = createClineNativeHooks('replay', {
  invoke(request) {
    if (request.tool === 'spawn_agent') return parentResult;
    assert.equal(request.tool, 'read_files');
    return Promise.resolve({json: JSON.stringify({output: 'contents'})});
  },
});
assert.equal(await replay.hooks.beforeTool(parent), undefined, 'delegation must execute');
assert.equal((await replay.hooks.beforeTool(child)).skip, true, 'native file read must not execute');
assert.deepEqual((await replay.hooks.afterTool({...child, result: {isError: true}})).result, {output: 'contents'});
finishDelegate({json: JSON.stringify({output: 'done'})});
await replay.hooks.afterTool(parent);
replay.assertSettled();

const broken = createClineNativeHooks('replay', {invoke: async () => { throw new Error('broker unavailable'); }});
await assert.rejects(broken.hooks.beforeTool(child), /broker unavailable/);
assert.throws(() => broken.assertSettled(), /capture or replay failed/);

const unfinished = createClineNativeHooks('record', transport);
await unfinished.hooks.beforeTool(child);
assert.throws(() => unfinished.assertSettled(), /did not settle/);

let invalidated = false;
const failedBoundary = createClineNativeHooks('record', {
  start: async () => 7,
  begin: async () => { throw new Error('snapshot unavailable'); },
  invalidate: async token => { assert.equal(token, 7); invalidated = true; },
});
await assert.rejects(failedBoundary.hooks.beforeTool(child), /snapshot unavailable/);
assert.equal(invalidated, true);
assert.throws(() => failedBoundary.assertSettled(), /capture or replay failed/);

const mismatch = createClineNativeHooks('replay', {invoke: async () => ({json: JSON.stringify({output: 'different'})})});
await mismatch.hooks.beforeTool(parent);
await assert.rejects(mismatch.hooks.afterTool(parent), /delegated tool result differs/);
assert.throws(() => mismatch.assertSettled(), /capture or replay failed/);
console.log('Cline native hooks: nested delegation, offline results, failure and settlement passed');
