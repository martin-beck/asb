import assert from 'node:assert/strict';
import {mkdtempSync, readdirSync, readFileSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createServer} from 'node:net';
import {spawn} from 'node:child_process';
import {NativeToolBroker} from '../assets/scripts/native-tool-broker.mjs';

const root = mkdtempSync(join(tmpdir(), 'asb-claude-hook-'));
const socket = join(root, 'broker.sock'), markerPath = join(root, 'marker.sock');
const markers = [];
const marker = createServer(client => {
  let input = '';
  client.on('data', part => {
    input += part;
    if (input.includes('\n')) { markers.push(input.trim()); client.end('1\n'); }
  });
});
const moduleUrl = new URL('../assets/scripts/claude-native-hook.mjs', import.meta.url).href;
function hook(event) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['--input-type=module', '-e', `import(${JSON.stringify(moduleUrl)}).then(m=>m.main()).catch(e=>{console.error(e.message);process.exitCode=1})`], {
      env: {...process.env, ASB_CLAUDE_NATIVE_DIRECTORY: root, ASB_CLAUDE_NATIVE_SOCKET: socket, ASB_CLAUDE_NATIVE_MARKER: markerPath},
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let out = '', err = '';
    child.stdout.on('data', data => out += data);
    child.stderr.on('data', data => err += data);
    child.on('error', reject);
    child.on('close', code => resolve({code, out, err}));
    child.stdin.end(JSON.stringify(event));
  });
}
try {
  await new Promise(resolve => marker.listen(markerPath, resolve));
  const broker = new NativeToolBroker('record');
  await broker.listen(socket);
  const base = {session_id: 'session', tool_use_id: 'call', tool_name: 'Read', tool_input: {file_path: '/fixture'}};
  assert.equal((await hook({...base, hook_event_name: 'PreToolUse'})).code, 0);
  const after = await hook({...base, hook_event_name: 'PostToolUse', tool_response: {z: 1, a: 2}});
  assert.equal(after.code, 0, after.err);
  assert.deepEqual(JSON.parse(after.out), {});
  const failure = {...base, tool_use_id: 'failed-call', tool_name: 'Bash', tool_input: {command: 'fixture'}};
  assert.equal((await hook({...failure, hook_event_name: 'PreToolUse'})).code, 0);
  assert.equal((await hook({...failure, hook_event_name: 'PostToolUseFailure', error: 'fixture failure', is_interrupt: false})).code, 0);
  const events = await broker.finish();
  assert.equal(events.length, 2);
  assert.deepEqual(events[1].outcome.value, {kind: 'failure', json: '{\"error\":\"fixture failure\",\"is_interrupt\":false}'});
  assert.deepEqual(events[0].request, {tool: 'Read', input: {file_path: '/fixture'}});
  assert.equal(events[0].outcome.value.json, '{"z":1,"a":2}');
  assert.match(markers[0], /^tool-begin:claude-code-0:\d+:\d+$/);
  assert.match(markers[1], /^tool-end:claude-code-0:\d+:\d+$/);
  assert.equal(readdirSync(root).filter(name => name.endsWith('.json')).length, 0);
  const failed = await hook({...base, hook_event_name: 'PostToolUse', tool_response: {}});
  assert.notEqual(failed.code, 0);
  assert.match(readFileSync(join(root, 'hook-failed'), 'utf8'), /failed/);
  await new Promise(resolve => marker.close(resolve));
  console.log('Claude native hook: separate-process recording and failure state passed');
} finally { rmSync(root, {recursive: true, force: true}); }
