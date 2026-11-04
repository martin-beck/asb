// Adapter-owned prototype lifecycle. Its caller must establish process-tree
// quiescence and bind the resulting events to actual settled snapshot data
// before publishing a complete recording.
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { ContinueBroker } from './continue-native-broker.mjs';
import { replayEffects } from './continue-snapshot-boundary.mjs';

export async function startContinue({ node, cli, args, cwd, env, bundle, socket, marker, mode, events = [], stdio = 'inherit' }) {
  if (!node || !cli || !bundle || !socket || !cwd || !Array.isArray(args) || !env || !['record', 'replay'].includes(mode)) {
    throw new TypeError('Continue supervisor configuration is incomplete');
  }
  const broker = new ContinueBroker(mode, events, replayEffects(marker));
  await broker.listen(socket);
  const bootstrap = fileURLToPath(new URL('./continue-native-bootstrap.mjs', import.meta.url));
  const options = [env.NODE_OPTIONS, `--import=${JSON.stringify(bootstrap)}`].filter(Boolean).join(' ');
  const childEnv = { ...env, NODE_OPTIONS: options, ASB_CONTINUE_BUNDLE: bundle,
    ASB_CONTINUE_MODE: mode, ASB_CONTINUE_SOCKET: socket };
  if (marker) childEnv.ASB_CONTINUE_MARKER = marker;
  else delete childEnv.ASB_CONTINUE_MARKER;
  let child;
  try {
    child = spawn(node, [cli, ...args], { cwd, stdio, env: childEnv });
  } catch (error) { await broker.abort(); throw error; }
  // Register immediately so an early spawn failure or exit is never missed.
  const exited = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', (code, signal) => resolve({ code, signal }));
  });
  return { child, exited, broker };
}
