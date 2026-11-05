import { registerHooks } from 'node:module';
import { realpathSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createNativeDispatcher } from './native-tool-dispatch.mjs';
import { recordBoundaries } from './native-snapshot-boundary.mjs';
import { transformQwenDispatch, transformQwenProvider } from './qwen-native-transform.mjs';
import { installQwenToolContext } from './qwen-native-context.mjs';

const { ASB_NATIVE_BUNDLE: bundle, ASB_NATIVE_MODE: mode, ASB_NATIVE_SOCKET: socket, ASB_NATIVE_MARKER: marker } = process.env;
if (!bundle || !socket || !marker || !['record', 'replay'].includes(mode)) throw new Error('Qwen native bootstrap requires the bundle, broker and snapshot marker');
const expected = pathToFileURL(realpathSync(bundle)).href;
const provider = pathToFileURL(realpathSync(join(dirname(bundle), 'chunk-TR3TAG6E.js'))).href;
const toolContext = mode === 'record' ? installQwenToolContext(process.env.OPENAI_BASE_URL || process.env.ANTHROPIC_BASE_URL) : undefined;
const replayBase = process.env.OPENAI_BASE_URL || process.env.ANTHROPIC_BASE_URL;
globalThis[Symbol.for('asb.qwen.toolScope.v1')] = toolContext?.scope ?? ((_call, invoke) => invoke());
globalThis[Symbol.for('asb.qwen.providerFetch.v1')] = toolContext?.wrapFetch ?? (fetch => fetch);
const dispatch = createNativeDispatcher(mode, socket, {
  // A delegated agent must actually run so its own tool calls reach this hook.
  isDelegation: call => call.request.name.toLowerCase() === 'agent',
  toRequest: call => ({ tool: call.request.name, args: call.request.args }),
  ...recordBoundaries(marker, 'qwen-code'),
  snapshotOrder: true,
  ...(toolContext ? { invokeRecorded: toolContext.invokeRecorded } : {}),
  ...(mode === 'replay' ? { beforeReplay: async ordinal => {
    if (!replayBase) throw new Error('Qwen replay provider endpoint is missing');
    const response = await fetch(`${replayBase.replace(/\/+$/, '')}/__asb_native_tool_consumed`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ agent: 'qwen-code', ordinal }),
    });
    if (!response.ok) throw new Error(`Qwen replay could not settle nested provider exchanges (HTTP ${response.status})`);
  } } : {}),
});
globalThis[Symbol.for('asb.qwen.nativeDispatch.v1')] = (call, invoke) => dispatch(call, null, invoke);
registerHooks({load(url, context, nextLoad) {
  const loaded = nextLoad(url, context);
  if (url !== expected && url !== provider) return loaded;
  if (loaded.source == null) throw new Error('Qwen native scheduler source is unavailable');
  return { ...loaded, source: url === expected ? transformQwenDispatch(loaded.source.toString()) : transformQwenProvider(loaded.source.toString()) };
}});
