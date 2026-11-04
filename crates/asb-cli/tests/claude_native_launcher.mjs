import assert from 'node:assert/strict';
import {mkdtemp, mkdir, readFile, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {launchClaude} from '../assets/scripts/claude-native-launcher.mjs';
import {startWebFetchReplayService} from '../assets/scripts/claude-webfetch-service.mjs';

const root = await mkdtemp(join(tmpdir(), 'asb-claude-launch-'));
try {
  const runtime = join(root, 'runtime'), state = join(runtime, 'hook-state');
  await mkdir(runtime); await mkdir(state);
  const service = await startWebFetchReplayService({runtime, plan: []});
  const ready = join(runtime, 'ready.json'), prior = join(root, 'prior.pem'), output = join(root, 'child.json');
  await writeFile(ready, JSON.stringify({address: service.address, certPath: service.certPath}));
  await writeFile(prior, 'fixture previous trust');
  const environment = {...process.env, ASB_CLAUDE_NATIVE_DIRECTORY: state, ASB_CLAUDE_WEB_READY: ready,
    ANTHROPIC_BASE_URL: 'http://172.19.0.4:4312/v1', NODE_EXTRA_CA_CERTS: prior, FIXTURE_OUTPUT: output};
  const code = `import {writeFileSync} from 'node:fs'; writeFileSync(process.env.FIXTURE_OUTPUT, JSON.stringify({proxy: process.env.HTTPS_PROXY, extra: process.env.NODE_EXTRA_CA_CERTS, noProxy: process.env.NO_PROXY, webProxy: process.env.ASB_CLAUDE_NATIVE_WEB_PROXY}));`;
  const result = await launchClaude({program: process.execPath, args: ['--input-type=module', '-e', code], environment, stdio: 'ignore'});
  assert.deepEqual(result, {code: 0, signal: null});
  const child = JSON.parse(await readFile(output));
  assert.equal(child.proxy, service.address); assert.equal(child.webProxy, service.address);
  assert.match(child.noProxy, /172\.19\.0\.4/);
  const trust = await readFile(child.extra, 'utf8');
  assert.match(trust, /^fixture previous trust\n/); assert.match(trust, /BEGIN CERTIFICATE/);
  await service.finish();
  const badRuntime = join(root, 'bad'), badState = join(badRuntime, 'state');
  await mkdir(badRuntime); await mkdir(badState);
  const badReady = join(badRuntime, 'ready.json');
  await writeFile(badReady, JSON.stringify({address: 'http://example.test:1234/', certPath: prior}));
  await assert.rejects(launchClaude({program: process.execPath, args: [], environment: {...environment, ASB_CLAUDE_NATIVE_DIRECTORY: badState, ASB_CLAUDE_WEB_READY: badReady}, stdio: 'ignore'}), /endpoint is invalid/);
  console.log('Claude native launcher: local proxy and preserved extra trust passed');
} finally { await rm(root, {recursive: true, force: true}); }
