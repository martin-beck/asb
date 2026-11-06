import assert from 'node:assert/strict';
import { RecordingSession, ReplaySession } from '../assets/scripts/native-tool-session.mjs';
import { encodeSidecar } from '../assets/scripts/native-tool-sidecar.mjs';
import { replayBroker } from '../assets/scripts/native-tool-custody.mjs';

const request = id => ({tool:'bash', args:{command:'same'}, call_id:id});
const exchange = (ordinal, req, value) => ({ordinal, request:req, outcome:{ok:true,value}});

// Calls with identical tool arguments can arrive in the opposite order. Their
// recorded IDs, outcomes and snapshot effects must stay associated.
const effects = [];
const modern = new ReplaySession([
  exchange(1, request('call-b'), 'result-b'),
  exchange(0, request('call-a'), 'result-a'),
], async event => effects.push(event.ordinal), {agent:'pi'});
const b = modern.invoke(request('call-b'));
const a = modern.invoke(request('call-a'));
assert.deepEqual(await Promise.all([a,b]), ['result-a','result-b']);
assert.deepEqual(effects, [1,0]);
modern.finish();

// Existing Pi cassettes did not store IDs. Sequential matching remains usable.
const oldRequest = {tool:'bash', args:{command:'same'}};
const legacyEvents = [exchange(0,oldRequest,'first'), exchange(1,oldRequest,'second')];
const legacy = new ReplaySession(legacyEvents, async()=>{}, {agent:'pi'});
assert.equal(await legacy.invoke(request('new-id-a')), 'first');
assert.equal(await legacy.invoke(request('new-id-b')), 'second');
legacy.finish();

// The real sealed-sidecar custody path must activate the same Pi-only fallback.
const legacySidecar = encodeSidecar(legacyEvents, 'a'.repeat(64), 'b'.repeat(64), 'pi');
const legacyBroker = replayBroker(legacySidecar, {programSha256:'a'.repeat(64), workspaceEffectSha256:'b'.repeat(64), applyEffects:async()=>{}, agent:'pi'});
assert.equal(await legacyBroker.session.invoke(request('new-id-a')), 'first');
assert.equal(await legacyBroker.session.invoke(request('new-id-b')), 'second');
legacyBroker.session.finish();

// The old format cannot disambiguate overlapping identical calls. Reject the
// replay rather than silently assigning an arbitrary result to either call.
const ambiguous = new ReplaySession(legacyEvents, async()=>{}, {agent:'pi'});
const first = ambiguous.reserve(request('new-id-a'));
await first.assignment;
assert.throws(() => ambiguous.reserve(request('new-id-b')), /ambiguous concurrent/);
await assert.rejects(first.outcome, /ambiguous concurrent/);
assert.throws(() => ambiguous.finish(), /incomplete/);

const duplicateId = new ReplaySession([
  exchange(0,request('same-id'),'first'), exchange(1,request('same-id'),'second'),
], async()=>{}, {agent:'pi'});
const existing = duplicateId.reserve(request('same-id'));
await existing.assignment;
assert.throws(() => duplicateId.reserve(request('same-id')), /ambiguous concurrent/);
await assert.rejects(existing.outcome, /ambiguous concurrent/);

// Fallback is scoped to Pi; other adapters must not ignore a changed field.
const other = new ReplaySession([exchange(0,oldRequest,'first')], async()=>{}, {agent:'continue'});
await assert.rejects(other.invoke(request('unexpected')), /no matching/);
assert.throws(() => other.finish(), /incomplete/);
assert.throws(() => new ReplaySession([exchange(0,oldRequest,'one'), exchange(1,request('new'),'two')], async()=>{}, {agent:'pi'}), /inconsistent/);
assert.throws(() => new ReplaySession([exchange(0,{...oldRequest,call_id:''},'one')], async()=>{}, {agent:'pi'}), /invalid tool call identities/);
const recording = new RecordingSession({agent:'pi'});
assert.equal(await recording.invoke(oldRequest, async()=>42),42, 'missing identity must not suppress the actual tool');
assert.throws(() => recording.finish(), /lacks stable tool call identities/);
console.log('Pi native tool identity and legacy compatibility checks passed');
