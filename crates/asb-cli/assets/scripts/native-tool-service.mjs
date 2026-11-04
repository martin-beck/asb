// Adapter-owned recording service, launched as a sibling of the agent by its
// process owner. Only the owner sends S after agent descendants have stopped.
import { createReadStream } from 'node:fs';
import { chmod, open, readFile, readdir, unlink } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { NativeToolBroker } from './native-tool-broker.mjs';
import { sealPendingBroker, replayBroker } from './native-tool-custody.mjs';
import { replayEffects } from './native-snapshot-boundary.mjs';

const [socket, program, target, archive, marker] = process.argv.slice(2);
const agent = process.env.ASB_NATIVE_TOOL_AGENT || 'continue';
if (!/^[a-z][a-z0-9-]{0,31}$/.test(agent)) throw new Error('native tool service agent is invalid');
const mode = process.argv.length === 5 ? 'record' : process.argv.length === 7 ? 'replay' : '';
if (!socket || !program || !target || !mode || (mode === 'replay' && (!archive || !marker))) throw new Error('Native tool native service configuration is incomplete');
async function digest(path) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
}
const programSha256 = await digest(program);
const snapshotSha256 = mode === 'replay' && archive.startsWith('sha256:') ? archive.slice(7) : mode === 'replay' ? await digest(archive) : '';
if (mode === 'replay' && !/^[a-f0-9]{64}$/.test(snapshotSha256)) throw new Error('Native tool replay snapshot digest is invalid');
const broker = mode === 'record' ? new NativeToolBroker('record') : replayBroker(await readFile(target), {
  programSha256, workspaceEffectSha256: snapshotSha256, applyEffects: replayEffects(marker, agent), agent,
});
await broker.listen(socket);
if (agent === 'claude-code') await chmod(socket, 0o666);
let webService;
const webPlan = process.env.ASB_CLAUDE_WEB_PLAN;
const webReady = process.env.ASB_CLAUDE_WEB_READY;
if (webPlan || webReady) {
  if (agent !== 'claude-code' || mode !== 'replay' || !webPlan || !webReady || !process.env.ASB_CLAUDE_WEB_RUNTIME)
    throw new Error('Claude WebFetch replay service configuration is incomplete');
  const plan = JSON.parse(await readFile(webPlan, 'utf8'));
  const {startWebFetchReplayService} = await import('./claude-webfetch-service.mjs');
  webService = await startWebFetchReplayService({runtime: process.env.ASB_CLAUDE_WEB_RUNTIME, plan});
  try {
    const file = await open(webReady, 'wx', 0o644);
    try { await file.writeFile(JSON.stringify({address: webService.address, certPath: webService.certPath})); await file.sync(); }
    finally { await file.close(); }
    await chmod(webReady, 0o644);
  } catch (error) { await webService.abort().catch(() => {}); throw error; }
}
process.stdout.write('READY\n');
try {
  const chunks = [];
  for await (const chunk of process.stdin) {
    chunks.push(chunk);
    if (chunks.reduce((sum, item) => sum + item.length, 0) > 2) throw new Error('Native tool service settlement request is invalid');
  }
  if (Buffer.concat(chunks).toString() !== 'S\n') throw new Error('Native tool service stopped without process-tree settlement');
  if (agent === 'claude-code') {
    const state = process.env.ASB_NATIVE_HOOK_STATE;
    if (!state || (await readdir(state)).length) throw new Error('Claude native hook state is missing or unfinished');
  }
  if (mode === 'record') {
    const pending = await sealPendingBroker(broker, { programSha256, agent });
    const file = await open(target, 'wx', 0o600);
    try { await file.writeFile(pending); await file.sync(); } finally { await file.close(); }
  } else await broker.finish();
  if (webService) await webService.finish();
} catch (error) {
  await broker.abort().catch(() => {});
  if (webService) await webService.abort().catch(() => {});
  throw error;
} finally {
  if (webReady) await unlink(webReady).catch(() => {});
}
