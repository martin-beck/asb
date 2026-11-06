import assert from 'node:assert/strict';
import { RecordingSession, ReplaySession } from '../assets/scripts/native-tool-session.mjs';

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
await assert.rejects(incomplete.invoke({ tool: 'Fetch', args: { url: 'fixture' } }), /closed/);
assert.throws(() => incomplete.finish(), /incomplete/);
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

// Hooks that occur in separate processes must reserve on entry and release at
// completion, including when a child finishes before its parent.
const twoStageEffects = [];
const twoStage = new ReplaySession([
  {ordinal: 1, request: {tool: 'child'}, outcome: {ok: true, value: 'child result'}},
  {ordinal: 0, request: {tool: 'parent'}, outcome: {ok: true, value: 'parent result'}},
], async event => { twoStageEffects.push(event.ordinal); });
const parentReservation = twoStage.reserve({tool: 'parent'});
const childReservation = twoStage.reserve({tool: 'child'});
assert.throws(() => twoStage.finish(), /incomplete/);
assert.deepEqual(twoStageEffects, []);
assert.deepEqual(await parentReservation.assignment, {ok: true, value: 'parent result'});
assert.deepEqual(await childReservation.assignment, {ok: true, value: 'child result'});
assert.deepEqual(twoStageEffects, [], 'inspection must not apply effects');
assert.equal(await childReservation.release(), 'child result');
assert.deepEqual(twoStageEffects, [1]);
assert.equal(await parentReservation.release(), 'parent result');
assert.deepEqual(twoStageEffects, [1, 0]);
assert.throws(() => parentReservation.release(), /unavailable/);
twoStage.finish();
assert.throws(() => twoStage.reserve({tool: 'parent'}), /closed/);
const missingReservation = new ReplaySession([{ordinal: 0, request: {tool: 'known'}, outcome: {ok: true, value: null}}]);
assert.throws(() => missingReservation.reserve({tool: 'unknown'}), /no matching/);
missingReservation.abort();
const failedReservation = new ReplaySession([
  {ordinal: 0, request: {tool: 'write'}, outcome: {ok: true, value: 'unused'}},
], async () => { throw new Error('snapshot failed'); });
const failedHandle = failedReservation.reserve({tool: 'write'});
await assert.rejects(failedHandle.release(), /snapshot failed/);
assert.throws(() => failedReservation.finish(), /incomplete/);

// An out-of-order duplicate must not wait forever for a single later event.
const duplicates = new ReplaySession([
  {ordinal: 0, request: {tool: 'first'}, outcome: {ok: true, value: 0}},
  {ordinal: 1, request: {tool: 'later'}, outcome: {ok: true, value: 1}},
]);
const later = duplicates.reserve({tool: 'later'});
const lateResult = later.outcome;
lateResult.catch(() => {});
assert.throws(() => duplicates.reserve({tool: 'later'}), /no matching/);
await assert.rejects(lateResult, /no matching/);
assert.throws(() => duplicates.reserve({tool: 'first'}), /closed/);
assert.throws(() => duplicates.finish(), /incomplete/);

// An extra request after every recorded tool has completed is still a replay
// mismatch, even if the agent catches that request's error.
const extra = new ReplaySession([{ordinal: 0, request: {tool: 'known'}, outcome: {ok: true, value: 1}}]);
assert.equal(await extra.invoke({tool: 'known'}), 1);
await assert.rejects(extra.invoke({tool: 'extra'}), /no matching/);
assert.throws(() => extra.finish(), /incomplete/);
