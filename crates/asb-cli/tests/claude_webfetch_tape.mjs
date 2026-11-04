import assert from 'node:assert/strict';
import {extractWebFetchPage} from '../assets/scripts/claude-webfetch-tape.mjs';
const prefix = '\nWeb page content:\n---\n';
const suffix = '\n---\n\nfixture prompt\n\nProvide a concise response based only on the content above.\nFurther instructions';
const page = '<html>fixture café\n---\nordinary separator</html>\n';
const message = prefix + page + suffix;
assert.equal(extractWebFetchPage(message, 'fixture prompt', Buffer.byteLength(page)), page);
assert.throws(() => extractWebFetchPage(message, 'other prompt', Buffer.byteLength(page)), /missing or ambiguous/);
assert.throws(() => extractWebFetchPage(message, 'fixture prompt', Buffer.byteLength(page) - 1), /byte count/);
assert.throws(() => extractWebFetchPage(message.slice(1), 'fixture prompt', Buffer.byteLength(page)), /unsupported/);
assert.throws(() => extractWebFetchPage(prefix + page + suffix + suffix, 'fixture prompt', Buffer.byteLength(page)), /ambiguous/);
assert.throws(() => extractWebFetchPage(message, '', 0), /invalid/);
console.log('Claude WebFetch recorded page extraction passed');

const {planWebFetchReplay} = await import('../assets/scripts/claude-webfetch-tape.mjs');
const web = (ordinal, url, prompt, code, bytes) => ({ordinal, request: {tool: 'WebFetch', input: {url, prompt}}, outcome: {ok: true, value: {kind: 'success', json: JSON.stringify({url, code, bytes})}}});
const tape = [{request: {body: {messages: [{role: 'user', content: [{type: 'text', text: message}]}], tools: []}}}];
const input = [web(1, 'https://example.test/ok', 'fixture prompt', 200, Buffer.byteLength(page)), web(0, 'https://example.test/redirect', 'redirect prompt', 302, 20)];
assert.deepEqual(planWebFetchReplay(input, tape), [
  {ordinal: 0, url: 'https://example.test/redirect', status: 302, redirect: true},
  {ordinal: 1, url: 'https://example.test/ok', status: 200, page},
]);
assert.throws(() => planWebFetchReplay(input, []), /missing or ambiguous/);
assert.throws(() => planWebFetchReplay(input, [...tape, ...tape]), /missing or ambiguous/);
assert.throws(() => planWebFetchReplay([web(1, 'https://example.test/ok', 'fixture prompt', 200, 4)], tape), /missing or ambiguous/);
assert.throws(() => planWebFetchReplay([web(1, 'file:///fixture', 'fixture prompt', 200, Buffer.byteLength(page))], tape), /does not match/);
assert.throws(() => planWebFetchReplay([web(1, 'https://example.test/ok', 'fixture prompt', 500, 0)], tape), /unsupported/);
