// Opt-in, offline integration probe using the installed Pi agent-core bundle.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { createServer as createHttpServer } from 'node:http';
import { once } from 'node:events';
import { mkdtempSync, appendFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';

const scripts = fileURLToPath(new URL('../assets/scripts', import.meta.url));
const bundle = process.env.ASB_TEST_PI_BUNDLE;
if (!bundle) throw new Error('ASB_TEST_PI_BUNDLE must name the installed Pi agent-loop.js');

if (process.argv[2] === 'child') {
  const { runAgentLoop } = await import(pathToFileURL(bundle));
  let turns = 0;
  const completion = [];
  const tools = ['one', 'two'].map(name => ({
    name, label: name, description: name,
    parameters: { type: 'object', properties: {}, additionalProperties: false },
    execute: async () => {
      if (name === 'one' && process.env.PROBE_URL) {
        assert.equal(await (await fetch(process.env.PROBE_URL)).text(), 'recorded fixture');
      }
      appendFileSync(process.env.PROBE_EFFECT, name + '\n');
      return { content: [{ type: 'text', text: name }], details: {} };
    },
  }));
  const assistant = content => ({
    role: 'assistant', content, api: 'openai-completions', provider: 'fixture', model: 'fixture',
    usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    stopReason: content.some(item => item.type === 'toolCall') ? 'toolUse' : 'stop', timestamp: 1,
  });
  const stream = async () => {
    const message = turns++ === 0
      ? assistant([{ type: 'toolCall', id: 'a', name: 'one', arguments: {} },
                   { type: 'toolCall', id: 'b', name: 'two', arguments: {} }])
      : assistant([{ type: 'text', text: 'done' }]);
    return { async *[Symbol.asyncIterator]() { yield { type: 'done' }; }, result: async () => message };
  };
  const config = { model: { provider: 'fixture', id: 'fixture' }, toolExecution: 'parallel', convertToLlm: async messages => messages };
  const prompts = [{ role: 'user', content: [{ type: 'text', text: 'fixture' }], timestamp: 1 }];
  await runAgentLoop(prompts, { systemPrompt: 'fixture', messages: [], tools }, config,
    async event => { if (event.type === 'tool_execution_end') completion.push(event.toolName); }, undefined, stream);
  process.stdout.write(JSON.stringify({ turns, completion }) + '\n');
} else {
  const { NativeToolBroker } = await import(pathToFileURL(join(scripts, 'native-tool-broker.mjs')));
  const root = mkdtempSync(join(tmpdir(), 'asb-pi-real-order-'));
  const marker = join(root, 'marker.sock');
  const effect = join(root, 'effects');
  let requests = 0, upstream;
  if (process.env.ASB_TEST_PI_HTTP === '1') {
    upstream = createHttpServer((_request, response) => { requests++; response.end('recorded fixture'); });
    await new Promise(resolve => upstream.listen(0, '127.0.0.1', resolve));
  }
  const url = upstream ? `http://127.0.0.1:${upstream.address().port}/` : '';
  let held, activeBroker;
  const snapshotOrder = [];
  const markerServer = createServer(client => client.once('data', data => {
    const text = data.toString();
    if (text.startsWith('tool-begin:')) return client.end('1\n');
    const match = /^tool-end:pi-(\d+):/.exec(text);
    if (!match) return client.destroy();
    const ordinal = Number(match[1]);
    snapshotOrder.push(ordinal);
    if (snapshotOrder.length === 1) { held = client; return; }
    client.end('3\n');
    // Release the first marker only after the second result reached the
    // broker, forcing the observed completion order to differ from snapshots.
    let attempts = 0;
    const timer = setInterval(() => {
      if (!activeBroker.pending.has(ordinal) || ++attempts >= 1000) {
        clearInterval(timer);
        held.end('2\n');
      }
    }, 5);
  }));
  await new Promise(resolve => markerServer.listen(marker, resolve));
  async function run(mode, events = [], id = '') {
    const applied = [];
    const broker = new NativeToolBroker(mode, events, async event => { applied.push(event.ordinal); });
    if (mode === 'record') activeBroker = broker;
    const socket = join(root, `broker-${id || 'record'}.sock`);
    await broker.listen(socket);
    const env = { PATH:process.env.PATH || '/usr/bin:/bin', HOME:root, ASB_TEST_PI_BUNDLE:bundle,
      NODE_OPTIONS: `--import=${scripts}/pi-native-bootstrap.mjs`,
      ASB_NATIVE_BUNDLE: bundle, ASB_NATIVE_MODE: mode, ASB_NATIVE_SOCKET: socket,
      ASB_NATIVE_MARKER: marker, PROBE_EFFECT: effect, PROBE_URL: url };
    const child = spawn('unshare', [upstream && mode === 'record' ? '-Ur' : '-Urn', process.execPath, process.argv[1], 'child'],
      { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    const timeout = setTimeout(() => child.kill('SIGKILL'), 20000);
    try {
      const [code] = await once(child, 'exit');
      assert.equal(code, 0, stderr);
      return { value: JSON.parse(stdout), recorded: await broker.finish(), applied };
    } catch (error) {
      await broker.abort().catch(() => {});
      throw error;
    } finally { clearTimeout(timeout); }
  }
  try {
    const record = await run('record');
    if (upstream) {
      assert.equal(requests, 1, 'recording must reach the fixture HTTP server');
      await new Promise(resolve => upstream.close(resolve));
      upstream = null;
    }
    assert.equal(record.value.turns, 2);
    assert.deepEqual(record.recorded.map(event => event.snapshot_sequence), [3, 2]);
    assert.deepEqual(record.recorded.map(event => event.ordinal), [...snapshotOrder].reverse());
    const before = readFileSync(effect, 'utf8');
    const replays = await Promise.all([1, 2, 3, 4].map(id => run('replay', record.recorded, String(id))));
    for (const replay of replays) {
      assert.deepEqual(replay.applied, snapshotOrder);
      assert.deepEqual(replay.value.completion, record.value.completion);
    }
    assert.equal(readFileSync(effect, 'utf8'), before, 'replay executed a native tool');
    if (url) assert.equal(requests, 1, 'replay must not repeat the HTTP request');
    console.log('installed Pi core: crossed-order record and four parallel replays passed');
  } finally {
    if (upstream) await new Promise(resolve => upstream.close(resolve));
    await new Promise(resolve => markerServer.close(resolve));
    rmSync(root, { recursive: true, force: true });
  }
}
