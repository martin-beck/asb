// Adapter-owned recording service, launched as a sibling of the agent by its
// process owner. Only the owner sends S after agent descendants have stopped.
import { createReadStream } from 'node:fs';
import { open } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { ContinueBroker } from './continue-native-broker.mjs';
import { sealPendingBroker } from './continue-native-custody.mjs';

const [socket, program, destination] = process.argv.slice(2);
if (!socket || !program || !destination || process.argv.length !== 5) throw new Error('Continue recording service configuration is incomplete');
const hash = createHash('sha256');
for await (const chunk of createReadStream(program)) hash.update(chunk);
const programSha256 = hash.digest('hex');
const broker = new ContinueBroker('record');
await broker.listen(socket);
process.stdout.write('READY\n');
try {
  const chunks = [];
  for await (const chunk of process.stdin) {
    chunks.push(chunk);
    if (chunks.reduce((sum, item) => sum + item.length, 0) > 2) throw new Error('Continue service settlement request is invalid');
  }
  if (Buffer.concat(chunks).toString() !== 'S\n') throw new Error('Continue service stopped without process-tree settlement');
  const pending = await sealPendingBroker(broker, { programSha256 });
  const file = await open(destination, 'wx', 0o600);
  try { await file.writeFile(pending); await file.sync(); } finally { await file.close(); }
} catch (error) {
  await broker.abort().catch(() => {});
  throw error;
}
