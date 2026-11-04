// Adapter-owned recording service, launched as a sibling of the agent by its
// process owner. Only the owner sends S after agent descendants have stopped.
import { createReadStream } from 'node:fs';
import { open, readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { ContinueBroker } from './continue-native-broker.mjs';
import { sealPendingBroker, replayBroker } from './continue-native-custody.mjs';
import { replayEffects } from './continue-snapshot-boundary.mjs';

const [socket, program, target, archive, marker] = process.argv.slice(2);
const mode = process.argv.length === 5 ? 'record' : process.argv.length === 7 ? 'replay' : '';
if (!socket || !program || !target || !mode || (mode === 'replay' && (!archive || !marker))) throw new Error('Continue native service configuration is incomplete');
async function digest(path) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
}
const programSha256 = await digest(program);
const broker = mode === 'record' ? new ContinueBroker('record') : replayBroker(await readFile(target), {
  programSha256, workspaceEffectSha256: await digest(archive), applyEffects: replayEffects(marker),
});
await broker.listen(socket);
process.stdout.write('READY\n');
try {
  const chunks = [];
  for await (const chunk of process.stdin) {
    chunks.push(chunk);
    if (chunks.reduce((sum, item) => sum + item.length, 0) > 2) throw new Error('Continue service settlement request is invalid');
  }
  if (Buffer.concat(chunks).toString() !== 'S\n') throw new Error('Continue service stopped without process-tree settlement');
  if (mode === 'record') {
    const pending = await sealPendingBroker(broker, { programSha256 });
    const file = await open(target, 'wx', 0o600);
    try { await file.writeFile(pending); await file.sync(); } finally { await file.close(); }
  } else await broker.finish();
} catch (error) {
  await broker.abort().catch(() => {});
  throw error;
}
