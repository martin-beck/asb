// Local test harness for ASB's actual experimental broker, not production code.
import { readFile, writeFile, copyFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';

const [modulePath, mode, socket, eventsPath, snapshot, destination] = process.argv.slice(2);
const { NativeToolBroker } = await import(pathToFileURL(modulePath));
const events = mode === 'record' ? [] : JSON.parse(await readFile(eventsPath, 'utf8'));
const apply = mode === 'replay' ? async (event) => {
  if (process.env.ASB_TEST_MARKER) {
    const { snapshotBoundary } = await import(pathToFileURL(process.env.ASB_TEST_BOUNDARY_MODULE));
    await snapshotBoundary(process.env.ASB_TEST_MARKER, 'replay', 'end', event.ordinal, 'goose');
  } else await copyFile(snapshot, destination);
} : undefined;
const broker = new NativeToolBroker(mode, events, apply);
await broker.listen(socket);
process.stdout.write('READY\n');
for await (const line of createInterface({ input: process.stdin })) {
  if (line !== 'STOP') throw new Error('invalid control');
  const recorded = await broker.finish();
  if (mode === 'record') await writeFile(eventsPath, JSON.stringify(recorded));
  process.stdout.write('DONE\n');
  break;
}
