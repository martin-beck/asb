// Opt-in local-provider probe of the actual installed Qwen native scheduler.
import assert from 'node:assert/strict';
import { createServer as httpServer } from 'node:http';
import { createServer as netServer } from 'node:net';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, existsSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { NativeToolBroker } from '../assets/scripts/native-tool-broker.mjs';
import { replayEffects } from '../assets/scripts/native-snapshot-boundary.mjs';

const { ASB_TEST_QWEN_CLI: cli, ASB_TEST_QWEN_BUNDLE: bundle, ASB_TEST_QWEN_PROBE_DIR: base } = process.env;
if (!cli || !bundle || !base) throw new Error('Set ASB_TEST_QWEN_CLI, ASB_TEST_QWEN_BUNDLE and ASB_TEST_QWEN_PROBE_DIR');
const root = mkdtempSync(join(base, 'qwen-native-'));
const marker = join(root, 'marker.sock'), effect = join(root, 'effect');
let sequence = 1, calls = 0;
const markerServer = netServer(client => client.once('data', data => {
  if (data.toString().startsWith('tool-end:')) sequence++;
  client.end(`${sequence}\n`);
}));
await new Promise(resolve => markerServer.listen(marker, resolve));
const provider = httpServer(async (request, response) => {
  let raw = '';
  for await (const chunk of request) raw += chunk;
  if (request.url.endsWith('/__asb_native_tool_consumed')) {
    assert.deepEqual(JSON.parse(raw), {agent:'qwen-code',ordinal:0});
    response.writeHead(204); response.end(); return;
  }
  const input = JSON.parse(raw);
  const completed = (input.messages || []).some(message => message.role === 'tool');
  calls++;
  const delta = completed ? {content:'done'} : {tool_calls:[{index:0,id:'call_probe',type:'function',function:{name:'run_shell_command',arguments:JSON.stringify({command:`printf ok > ${effect}`})}}]};
  response.writeHead(200, {'Content-Type':'text/event-stream'});
  for (const choice of [{index:0,delta,finish_reason:null},{index:0,delta:{},finish_reason:completed?'stop':'tool_calls'}]) {
    response.write(`data: ${JSON.stringify({id:'chatcmpl-fixture',object:'chat.completion.chunk',created:1,model:'fixture',choices:[choice]})}\n\n`);
  }
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
const bootstrap = fileURLToPath(new URL('../assets/scripts/qwen-native-bootstrap.mjs', import.meta.url));
async function execute(mode, index, events=[]) {
  const socket = join(root, `${mode}-${index}.sock`), home = join(root, `${mode}-${index}`);
  mkdirSync(home);
  const broker = new NativeToolBroker(mode, events, replayEffects(marker, 'qwen-code'));
  await broker.listen(socket);
  const env = {PATH:process.env.PATH,HOME:home,TERM:'dumb',NO_COLOR:'1',
    QWEN_DEFAULT_AUTH_TYPE:'openai',OPENAI_API_KEY:'fixture',OPENAI_MODEL:'fixture',
    OPENAI_BASE_URL:`http://127.0.0.1:${provider.address().port}/v1`,
    ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:mode,ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:marker,
    NODE_OPTIONS:`--import=${JSON.stringify(bootstrap)}`};
  const child = spawn(process.execPath,[cli,'--model','fixture','--output-format','json','--yolo','run one shell command'],
    {cwd:root,env,stdio:['ignore','pipe','pipe']});
  let stderr = '';
  child.stderr.on('data', chunk => stderr += chunk);
  child.stdout.resume();
  const deadline = setTimeout(() => child.kill('SIGKILL'), 45000);
  const [status] = await once(child, 'exit');
  clearTimeout(deadline);
  assert.equal(status, 0, stderr);
  return broker.finish();
}
try {
  const events = await execute('record', 0);
  assert.equal(events.length, 1);
  assert.ok(existsSync(effect));
  unlinkSync(effect);
  await Promise.all(Array.from({length:4}, (_, index) => execute('replay', index, events)));
  assert.equal(existsSync(effect), false, 'replay must not run the shell command');
  assert.ok(calls >= 10, 'all clients must reach the local mock provider');
  console.log(`Qwen native mock-provider record and four concurrent replays passed; evidence: ${root}`);
} finally {
  await new Promise(resolve => provider.close(resolve));
  await new Promise(resolve => markerServer.close(resolve));
}
