import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

const modulePath = new URL('../assets/scripts/goose-native-wire.mjs', import.meta.url);

test('accepts replica-local identity control without consuming provider traffic', async () => {
  const { normalizeRequest, controlRequest } = await import(`${modulePath}?case=control`);
  const control = (body, mode='replay', method='POST', url='/.asb/goose-session-map') => controlRequest({ body: Buffer.from(body), mode, method, url });
  assert.equal(control('{}', 'record'), false);
  assert.equal(control('{}', 'replay', 'GET'), false);
  assert.equal(control('{}'), false);
  assert.throws(() => control('{invalid'));
  assert.equal(control(' '.repeat(1025)), false);
  assert.equal(control('{}', 'replay', 'POST', '/v1/chat/completions'), undefined);
  assert.equal(control('{"live":"20261104_4","recorded":"20261104_2"}'), true);
  assert.throws(() => control('{"live":"bad","recorded":"20261104_2"}'));
  const result = normalizeRequest({body:{messages:[{role:'user',content:'Subagent ID: 20261104_4'}, {role:'user',content:'Subagent ID: 20261104_2'}]}});
  assert.equal(result.messages[0].content, result.messages[1].content);
});

test('aliases observed Goose sessions without collapsing distinct children', async () => {
  const { normalizeRequest } = await import(`${modulePath}?case=ids`);
  const body = { messages: [
    { role: 'user', content: 'Subagent ID: 20261104_2\n\nfixture' },
    { role: 'tool', content: 'Task 20261104_3 started in background' },
    { role: 'assistant', tool_calls: [{ function: { name:'load', arguments: '{"source":"20261104_3"}' } }] },
  ] };
  const result = normalizeRequest({ body });
  assert.match(result.messages[0].content, /<asb-goose-session-1>/);
  assert.match(result.messages[1].content, /<asb-goose-session-2>/);
  assert.match(result.messages[2].tool_calls[0].function.arguments, /<asb-goose-session-2>/);
  assert.match(body.messages[0].content, /20261104_2/);
});

test('maps recorded and live IDs to the same alias while preserving other sessions', async t => {
  const target = new URL('../../../target/', import.meta.url);
  mkdirSync(target, { recursive: true });
  const directory = mkdtempSync(join(target.pathname, 'goose-wire-'));
  const file = join(directory, 'ids.json');
  t.after(() => { delete process.env.ASB_GOOSE_ID_MAP_FILE; rmSync(directory, { recursive: true }); });
  process.env.ASB_GOOSE_ID_MAP_FILE = file;
  const { normalizeRequest } = await import(`${modulePath}?case=mapping`);
  const first = normalizeRequest({ body: { messages: [{ role:'user', content:'Subagent ID: 20261104_4' }] } });
  assert.match(first.messages[0].content, /<asb-goose-session-1>/);
  writeFileSync(file, JSON.stringify({ scope: { '20261104_4':'20261104_2' } }));
  const second = normalizeRequest({ body: { messages: [{ role:'tool', content:'Task 20261104_4 started in background' }, { role:'assistant', tool_calls:[{function:{name:'load',arguments:'{"source":"20261104_2"}'}}] }, { role:'user', content:'Subagent ID: 20261104_5' }] } });
  assert.match(second.messages[0].content, /<asb-goose-session-1>/);
  assert.match(second.messages[1].tool_calls[0].function.arguments, /<asb-goose-session-1>/);
  assert.match(second.messages[2].content, /<asb-goose-session-2>/);
});

test('keeps unrelated values and normalizes only observed task duration', async () => {
  const { normalizeRequest } = await import(`${modulePath}?case=scope`);
  const input = { messages: [
    { role: 'user', content: 'Keep identifier 20261104_9' },
    { role: 'user', content: 'A literal 20261104_2 must stay intact' },
    { role: 'tool', content: '# Background Task Result: 20261104_2\n**Duration:** 42s (2 turns)' },
    { role: 'user', content: '<turn-context>\nBackground tasks:\n• 20261104_2: "task" - running 9s, 2 turns, idle 3s\n</turn-context>\nKeep 20261104_2' },
  ] };
  const result = normalizeRequest({ body: input });
  assert.equal(result.messages[0].content, input.messages[0].content);
  assert.equal(result.messages[1].content, input.messages[1].content);
  assert.equal(result.messages[2].content, '# Background Task Result: <asb-goose-session-1>\n**Duration:** <duration>s (2 turns)');
  assert.equal(result.messages[3].content, '<turn-context>\nBackground tasks:\n• <asb-goose-session-1>: "task" - running <duration>s, 2 turns, idle <duration>s\n</turn-context>\nKeep 20261104_2');
});
