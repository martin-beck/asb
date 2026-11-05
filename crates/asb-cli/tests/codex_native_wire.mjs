import assert from 'node:assert/strict';
import { normalizeRequest, recordMetadata, observeReplayExchange, rewriteReplayResponse, configureReplay } from '../assets/scripts/codex-native-wire.mjs';
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
const longId = 'x'.repeat(1024);
const manyOutputs = Array.from({length: 17000}, () => ({type:'function_call_output',call_id:longId}));
const metadata = recordMetadata({url:'/v1/responses',body:{prompt_cache_key:longId,input:manyOutputs}});
assert.equal(metadata.conversation, longId);
assert.equal(metadata.output_call_ids.length, manyOutputs.length);
assert.equal(recordMetadata({url:'/v1/responses',body:{input:[{type:'function_call_output'}]}}), undefined);
const running = recordMetadata({url:'/v1/responses',body:{input:[{type:'function_call_output',call_id:'call',output:'Chunk ID: abc\nWall time: 1.25 seconds\nProcess running with session ID 1\nOutput:\n'}]}});
assert.deepEqual(running.running_wait_ms,[1500]);
assert.deepEqual(running.running_session_ids,['1']);
const recordedRunning = 'Chunk ID: abc\nWall time: 1.25 seconds\nProcess running with session ID 111\nOutput:\n';
const liveRunning = recordedRunning.replace('111','222');
const recordedInput = {input:[{type:'function_call_output',call_id:'call',output:recordedRunning},{type:'function_call',name:'write_stdin',arguments:'{"session_id":111,"yield_time_ms":1000}'}]};
const liveInput = {input:[{type:'function_call_output',call_id:'call',output:liveRunning},{type:'function_call',name:'write_stdin',arguments:'{"session_id":222,"yield_time_ms":1000}'}]};
assert.equal(normalizeRequest({url:'/v1/responses',body:structuredClone(recordedInput)}).input[1].arguments, normalizeRequest({url:'/v1/responses',body:structuredClone(liveInput)}).input[1].arguments.replace('process-2','process-1'));
observeReplayExchange({body:liveInput,interaction:{agent_record_metadata:{running_session_ids:['111']}}});
const call = {id:'item-1',type:'function_call',name:'write_stdin',arguments:'{"session_id":111,"yield_time_ms":1000}'};
const streamed = [
  {type:'response.function_call_arguments.delta',item_id:'item-1',delta:'{"session_'},
  {type:'response.function_call_arguments.delta',item_id:'item-1',delta:'id":111,"yield_time_ms":1000}'},
  {type:'response.function_call_arguments.done',item_id:'item-1',arguments:call.arguments},
  {type:'response.output_item.done',item:call},
  {type:'response.completed',response:{output:[call]}},
].map(value => `data: ${JSON.stringify(value)}\n\n`).join('');
const rewritten = rewriteReplayResponse({body:streamed}).split('\n').filter(line=>line.startsWith('data: ')).map(line=>JSON.parse(line.slice(6)));
assert.equal(JSON.parse(rewritten[0].delta + rewritten[1].delta).session_id,222);
assert.equal(JSON.parse(rewritten[2].arguments).session_id,222);
assert.equal(JSON.parse(rewritten[3].item.arguments).session_id,222);
assert.equal(JSON.parse(rewritten[4].response.output[0].arguments).session_id,222);
configureReplay({interactions:[{request:{}}]});
assert.equal(normalizeRequest({url:'/v1/responses',body:structuredClone(recordedInput)}).input[1].arguments,recordedInput.input[1].arguments);
assert.equal(rewriteReplayResponse({body:streamed}),streamed);
console.log('Codex generated input IDs are normalized only at their protocol locations');
