import test from 'node:test';
import assert from 'node:assert/strict';
import {normalizeComparison} from '../assets/scripts/agent-normalizers/claude-code.mjs';
import {normalizeToolResultsAsync} from '../assets/scripts/tool-result-normalization.mjs';

const calls = [{type:'tool_use',id:'b',name:'Read',input:{file_path:'b'}}, {type:'tool_use',id:'a',name:'Bash',input:{command:'test'}}];
const results = [{type:'tool_result',tool_use_id:'b',content:'file'}, {type:'tool_result',tool_use_id:'a',content:'output'}];
const body = (content) => ({messages:[{role:'assistant',content:calls}, {role:'user',content}]});

test('complete parallel results are ordered by their original call identity', () => {
  const expected = normalizeComparison({body:body(results)});
  assert.deepEqual(normalizeComparison({body:body([...results].reverse())}), expected);
  assert.notDeepEqual(normalizeComparison({body:body([{...results[0],content:'changed'}, results[1]])}), expected);
  assert.notDeepEqual(normalizeComparison({body:body([{...results[0],tool_use_id:'a'}, {...results[1],tool_use_id:'b'}])}), expected);
  const duplicate = body([results[0],results[0]]);
  assert.deepEqual(normalizeComparison({body:duplicate}), duplicate);
  const mixed = body([results[0],{type:'text',text:'keep'}]);
  assert.deepEqual(normalizeComparison({body:mixed}), mixed);
  const cache = {type:'ephemeral'};
  const first = body([results[0],{...results[1],cache_control:cache}]);
  const reversed = body([results[1],{...results[0],cache_control:cache}]);
  assert.deepEqual(normalizeComparison({body:first}), normalizeComparison({body:reversed}));
  const early = body([{...results[1],cache_control:cache},results[0]]);
  assert.deepEqual(normalizeComparison({body:early}), early);
});

test('tool-result profiles keep their associated tool after concurrent completion', async () => {
  const input = body([...results].reverse());
  const transformed = await normalizeToolResultsAsync(input, ({text,tool}) => `${tool.name}:${text}`, {associationBody:input});
  const ordered = normalizeComparison({body:transformed});
  assert.equal(ordered.messages[1].content[0].content, 'Read:file');
  assert.equal(ordered.messages[1].content[1].content, 'Bash:output');
});
