import assert from 'node:assert/strict';
import { normalizeRequest } from '../assets/scripts/codex-native-wire.mjs';
const a = 'msg_00112233-4455-6677-8899-aabbccddeeff';
const b = 'msg_11112233-4455-6677-8899-aabbccddeeff';
const c = 'fco_22222233-4455-6677-8899-aabbccddeeff';
const request = (first, second, output) => ({ input: [
  { type: 'message', role: 'developer', id: first, content: [{ text: first }] },
  { type: 'message', role: 'user', id: second },
  { type: 'function_call_output', id: output, output: first },
  { type: 'message', role: 'assistant', id: first },
  { type: 'function_call', id: output, arguments: JSON.stringify({ id: first }) },
] });
const input = request(a, b, c);
const normalized = normalizeRequest({ url: '/v1/responses', body: input });
assert.deepEqual(normalized.input.slice(0,3).map(item => item.id), ['<asb-codex-item-1>','<asb-codex-item-2>','<asb-codex-item-3>']);
assert.equal(normalized.input[0].content[0].text, a);
assert.equal(normalized.input[2].output, a);
assert.equal(normalized.input[3].id, a);
assert.equal(normalized.input[4].id, c);
assert.equal(JSON.parse(normalized.input[4].arguments).id, a);
const duplicate = normalizeRequest({ url: '/v1/responses?x=1', body: request(a, a, c) });
assert.equal(duplicate.input[0].id, duplicate.input[1].id);
assert.notEqual(duplicate.input[0].id, duplicate.input[2].id);
const unchanged = request(a,b,c);
assert.deepEqual(normalizeRequest({ url: '/v1/chat/completions', body: unchanged }), request(a,b,c));
const nonGenerated = { input: [{type:'message',role:'user',id:'msg_not-a-uuid'}, {type:'function_call_output',id:'fco_custom'}] };
assert.deepEqual(normalizeRequest({url:'/v1/responses', body: nonGenerated}), nonGenerated);
const uuid = '00112233-4455-6677-8899-aabbccddeeff';
const cached = normalizeRequest({url:'/v1/responses', body:{prompt_cache_key:uuid,input:[]}});
assert.equal(cached.prompt_cache_key, '<asb-codex-session-cache>');
assert.equal(normalizeRequest({url:'/v1/responses',body:{prompt_cache_key:'user-cache',input:[]}}).prompt_cache_key, 'user-cache');
assert.equal(normalizeRequest({url:'/v1/chat/completions',body:{prompt_cache_key:uuid}}).prompt_cache_key, uuid);
console.log('Codex generated input IDs are normalized only at their protocol locations');
