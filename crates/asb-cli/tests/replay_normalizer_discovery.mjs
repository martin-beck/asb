import test from 'node:test';
import assert from 'node:assert/strict';
import { findNormalizer } from '../assets/scripts/replay-normalizer-discovery.mjs';
import { normalizeToolResult } from '../assets/examples/normalizers/python-unittest-elapsed.mjs';

function request(text) {
  return {method: 'POST', url: '/v1/responses', body: {model: 'model', input: [
    {type: 'function_call', call_id: 'c1', name: 'bash', arguments: '{"command":"python -m unittest tests"}'},
    {type: 'function_call_output', call_id: 'c1', output: text},
  ]}};
}

test('finds an existing rule only if it makes the complete request match', () => {
  const left = request('Ran 1 test in 0.01s\n\nOK\n');
  const right = request('Ran 1 test in 0.02s\n\nOK\n');
  const candidates = [{id: 'unittest', normalizeToolResult}];
  assert.deepEqual(findNormalizer(left, right, candidates), {kind:'existing', id:'unittest'});
  right.body.model = 'another-model';
  assert.equal(findNormalizer(left, right, candidates), null);
});

test('generates an exact temporary rule for an observed tool-result pair', () => {
  const left = request('result 123');
  const right = request('result 456');
  const result = findNormalizer(left, right, []);
  assert.equal(result.kind, 'temporary');
  assert.match(result.source, /result 456/);
  assert.match(result.source, /result 123/);
  assert.equal(findNormalizer(left, {...right, url:'/other'}, []), null);
  right.body.input[0].arguments = '{"command":"different"}';
  assert.equal(findNormalizer(left, right, []), null);
});

test('rejects multiple differing outputs and ambiguous tool associations', () => {
  const left = request('first');
  const right = request('second');
  left.body.input.push({type:'function_call_output',call_id:'c1',output:'left'});
  right.body.input.push({type:'function_call_output',call_id:'c1',output:'right'});
  assert.equal(findNormalizer(left, right, []), null);
  left.body.input.splice(0, 1);
  right.body.input.splice(0, 1);
  assert.equal(findNormalizer(left, right, []), null);
});
