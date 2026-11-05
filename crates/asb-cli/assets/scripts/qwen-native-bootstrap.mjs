import { registerHooks } from 'node:module';
import { realpathSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { createNativeDispatcher } from './native-tool-dispatch.mjs';
import { recordBoundaries } from './native-snapshot-boundary.mjs';
import { transformQwenDispatch } from './qwen-native-transform.mjs';

const { ASB_NATIVE_BUNDLE: bundle, ASB_NATIVE_MODE: mode, ASB_NATIVE_SOCKET: socket, ASB_NATIVE_MARKER: marker } = process.env;
if (!bundle || !socket || !marker || !['record', 'replay'].includes(mode)) throw new Error('Qwen native bootstrap requires the bundle, broker and snapshot marker');
const expected = pathToFileURL(realpathSync(bundle)).href;
const dispatch = createNativeDispatcher(mode, socket, {
  // A delegated agent must actually run so its own tool calls reach this hook.
  isDelegation: call => call.request.name.toLowerCase() === 'agent',
  toRequest: call => ({ tool: call.request.name, args: call.request.args }),
  ...recordBoundaries(marker, 'qwen-code'),
  snapshotOrder: true,
});
globalThis[Symbol.for('asb.qwen.nativeDispatch.v1')] = (call, invoke) => dispatch(call, null, invoke);
registerHooks({load(url, context, nextLoad) {
  const loaded = nextLoad(url, context);
  if (url !== expected) return loaded;
  if (loaded.source == null) throw new Error('Qwen native scheduler source is unavailable');
  return { ...loaded, source: transformQwenDispatch(loaded.source.toString()) };
}});
