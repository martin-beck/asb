import { registerHooks } from 'node:module';
import { realpathSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { createNativeDispatcher } from './native-tool-dispatch.mjs';
import { recordBoundaries } from './native-snapshot-boundary.mjs';
import { transformPiDispatch } from './pi-native-transform.mjs';
import { installFetchDiagnostic } from './pi-fetch-diagnostic.mjs';

const { ASB_NATIVE_BUNDLE: bundle, ASB_NATIVE_MODE: mode, ASB_NATIVE_SOCKET: socket, ASB_NATIVE_MARKER: marker } = process.env;
if (!bundle || !socket || !marker || !['record','replay'].includes(mode)) throw new Error('Pi native bootstrap requires the bundle, broker and snapshot marker');
installFetchDiagnostic();
const expected = pathToFileURL(realpathSync(bundle)).href;
const dispatch = createNativeDispatcher(mode, socket, {
  toRequest: prepared => ({ tool: prepared.toolCall.name, args: prepared.args }),
  encodeResult: result => {
    if (result && Object.getPrototypeOf(result) === Object.prototype && result.details === undefined) {
      const { details: _details, ...remaining } = result;
      return remaining;
    }
    return result;
  },
  ...recordBoundaries(marker, 'pi'),
});
globalThis[Symbol.for('asb.pi.nativeDispatch.v1')] = (prepared, invoke) => dispatch(prepared, null, invoke);
registerHooks({load(url, context, nextLoad) {
  const loaded = nextLoad(url, context);
  if (url !== expected) return loaded;
  if (loaded.source == null) throw new Error('Pi native dispatch source is unavailable');
  return { ...loaded, source: transformPiDispatch(loaded.source.toString()) };
}});
