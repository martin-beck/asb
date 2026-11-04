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
function hook(event, mode = 'record') {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['--input-type=module', '-e', `import(${JSON.stringify(moduleUrl)}).then(m=>m.main()).catch(e=>{console.error(e.message);process.exitCode=1})`], {
      env: {...process.env, ASB_CLAUDE_NATIVE_DIRECTORY: root, ASB_CLAUDE_NATIVE_SOCKET: socket, ASB_CLAUDE_NATIVE_MARKER: markerPath, ASB_CLAUDE_NATIVE_MODE: mode},
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
  assert.equal((await hook({...failure, hook_event_name: 'PostToolUseFailure', error: 'Exit code 13\nfixture output', is_interrupt: false})).code, 0);
  const bash = {...base, tool_use_id: 'bash-call', tool_name: 'Bash', tool_input: {command: 'touch forbidden'}};
  assert.equal((await hook({...bash, hook_event_name: 'PreToolUse'})).code, 0);
  assert.equal((await hook({...bash, hook_event_name: 'PostToolUse', tool_response: {stdout: 'recorded', stderr: '', interrupted: false, isImage: false}})).code, 0);
  const events = await broker.finish();
  assert.equal(events.length, 3);
  assert.deepEqual(events[1].outcome.value, {kind: 'failure', json: '{\"error\":\"Exit code 13\\nfixture output\",\"is_interrupt\":false}'});
  assert.deepEqual(events[0].request, {tool: 'Read', input: {file_path: '/fixture'}});
  assert.equal(events[0].outcome.value.json, '{"z":1,"a":2}');
  assert.match(markers[0], /^tool-begin:claude-code-0:\d+:\d+$/);
  assert.match(markers[1], /^tool-end:claude-code-0:\d+:\d+$/);
  assert.equal(readdirSync(root).filter(name => name.endsWith('.json')).length, 0);
  const replayEffects = [];
  const playback = new NativeToolBroker('replay', [{...events[2], ordinal: 0}], async event => { replayEffects.push(event.ordinal); });
  await playback.listen(socket);
  const unsupported = await hook({...base, tool_name: 'WebFetch', hook_event_name: 'PreToolUse'}, 'replay');
  assert.notEqual(unsupported.code, 0);
  assert.match(unsupported.err, /Bash and Read only/);
  const before = await hook({...bash, hook_event_name: 'PreToolUse'}, 'replay');
  assert.equal(before.code, 0, before.err);
  assert.equal(JSON.parse(before.out).hookSpecificOutput.updatedInput.command, ':');
  assert.deepEqual(replayEffects, []);
  const replayAfter = await hook({...bash, tool_input: {command: ':'}, hook_event_name: 'PostToolUse', tool_response: {stdout: ''}}, 'replay');
  assert.equal(replayAfter.code, 0, replayAfter.err);
  assert.equal(JSON.parse(replayAfter.out).hookSpecificOutput.updatedToolOutput.stdout, 'recorded');
  assert.deepEqual(replayEffects, [0]);
  await playback.finish();
  const readEffects = [];
  const readPlayback = new NativeToolBroker('replay', [{...events[0], ordinal: 0}], async event => { readEffects.push(event.ordinal); });
  await readPlayback.listen(socket);
  const readBefore = await hook({...base, hook_event_name: 'PreToolUse'}, 'replay');
  assert.equal(readBefore.code, 0, readBefore.err);
  assert.deepEqual(JSON.parse(readBefore.out).hookSpecificOutput.updatedInput, base.tool_input);
  const readAfter = await hook({...base, hook_event_name: 'PostToolUse', tool_response: {ignored: true}}, 'replay');
  assert.equal(readAfter.code, 0, readAfter.err);
  assert.deepEqual(JSON.parse(readAfter.out).hookSpecificOutput.updatedToolOutput, {z: 1, a: 2});
  assert.deepEqual(readEffects, [0]);
  await readPlayback.finish();
  const failurePlayback = new NativeToolBroker('replay', [{...events[1], ordinal: 0}], async () => {});
  await failurePlayback.listen(socket);
  const failedBefore = await hook({...failure, hook_event_name: 'PreToolUse'}, 'replay');
  assert.equal(failedBefore.code, 0, failedBefore.err);
  const failedInput = JSON.parse(failedBefore.out).hookSpecificOutput.updatedInput;
  assert.match(failedInput.command, /exit 13$/);
  const failedAfter = await hook({...failure, tool_input: failedInput, hook_event_name: 'PostToolUseFailure', error: 'Exit code 13\nfixture output', is_interrupt: false}, 'replay');
  assert.equal(failedAfter.code, 0, failedAfter.err);
  assert.deepEqual(JSON.parse(failedAfter.out), {});
  await failurePlayback.finish();
  const failed = await hook({...base, hook_event_name: 'PostToolUse', tool_response: {}});
  assert.notEqual(failed.code, 0);
  assert.match(readFileSync(join(root, 'hook-failed'), 'utf8'), /failed/);
  await new Promise(resolve => marker.close(resolve));
  console.log('Claude native hook: separate-process recording and failure state passed');
} finally { rmSync(root, {recursive: true, force: true}); }
