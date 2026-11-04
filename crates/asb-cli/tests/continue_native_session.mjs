import assert from 'node:assert/strict';
import { RecordingSession, ReplaySession } from '../assets/scripts/continue-native-session.mjs';

let release;
const firstWait = new Promise(resolve => { release = resolve; });
const record = new RecordingSession();
const first = record.invoke({ tool: 'Read', args: { path: 'one' } }, async () => { await firstWait; return { content: 'first' }; });
const second = record.invoke({ tool: 'Fetch', args: { url: 'fixture' } }, async () => 'second');
assert.equal(await second, 'second');
assert.throws(() => record.finish(), /unfinished/);
release();
assert.deepEqual(await first, { content: 'first' });
const events = record.finish();
assert.deepEqual(events.map(event => event.ordinal), [1, 0]);
const effects = [];
const replay = new ReplaySession(events, async event => { effects.push(event.ordinal); });
const completion = [];
// Requests can arrive in the opposite dispatch order; neither replay invokes
// a live executor, and observations are delivered in recorded finish order.
const replaySecond = replay.invoke({ args: { url: 'fixture' }, tool: 'Fetch' }).then(value => { completion.push(1); return value; });
const replayFirst = replay.invoke({ args: { path: 'one' }, tool: 'Read' }).then(value => { completion.push(0); return value; });
assert.equal(await replaySecond, 'second');
assert.deepEqual(await replayFirst, { content: 'first' });
replay.finish();
assert.deepEqual(completion, [1, 0]);
assert.deepEqual(effects, [1, 0]);

const failed = new RecordingSession();
const original = new TypeError('fixture failure');
await assert.rejects(failed.invoke({ tool: 'Bash' }, async () => { throw original; }), error => error === original);
const failedReplay = new ReplaySession(failed.finish());
await assert.rejects(failedReplay.invoke({ tool: 'Bash' }), error => error.name === 'TypeError' && error.message === 'fixture failure');
failedReplay.finish();

const incomplete = new ReplaySession(events);
await assert.rejects(incomplete.invoke({ tool: 'unknown' }), /no matching/);
const blocked = incomplete.invoke({ tool: 'Fetch', args: { url: 'fixture' } });
assert.throws(() => incomplete.finish(), /incomplete/);
incomplete.abort();
await assert.rejects(blocked, /cancelled/);
assert.throws(() => new ReplaySession([{ ordinal: 1, request: {}, outcome: { ok: true, value: null } }]), /incomplete/);
console.log('Continue native session checks passed');

const unsupported = new RecordingSession();
await assert.rejects(unsupported.invoke({ tool: 'unsupported' }, async () => undefined), /not representable/);
assert.throws(() => unsupported.finish(), /unfinished/);
const effectFailure = new ReplaySession(events, async () => { throw new Error('effect failed'); });
const waitingA = effectFailure.invoke({ tool: 'Read', args: { path: 'one' } });
const waitingB = effectFailure.invoke({ tool: 'Fetch', args: { url: 'fixture' } });
await assert.rejects(waitingA, /effect failed/);
await assert.rejects(waitingB, /effect failed/);
