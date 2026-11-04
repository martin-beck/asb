import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import {main} from '../assets/scripts/cline-native-entry.mjs';

const env = {
  ASB_CLINE_CORE_MODULE: fileURLToPath(new URL('./fixtures/cline_native_fake_core.mjs', import.meta.url)),
  ASB_CLINE_NATIVE_SOCKET: '/fixture/broker', ASB_CLINE_MARKER: '/fixture/marker',
  ASB_CLINE_NATIVE_MODE: 'record', OPENAI_BASE_URL: 'http://127.0.0.1', CLINE_API_KEY: 'fixture-only',
};
const original = process.stdout.write;
let output = '';
try {
  process.stdout.write = chunk => { output += chunk; return true; };
  await main(['fixture-provider', 'fixture-model', 'fixture prompt', ''], env);
} finally { process.stdout.write = original; }
const lines = output.trim().split('\n').map(JSON.parse);
assert.deepEqual(lines[0], {type: 'agent_event', event: {type: 'fixture-event'}});
assert.equal(lines[1].type, 'run_result');
assert.equal(lines[1].finishReason, 'completed');
assert.equal(lines[1].aggregateUsage.inputTokens, 7);
assert(!output.includes('fixture-only'), 'credentials must not enter output');
await assert.rejects(main([], {}), /not prepared/);
console.log('Cline native entry: launch configuration, events and aggregate usage passed');
