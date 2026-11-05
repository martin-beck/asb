// Loaded into the pinned Continue process and any Node descendants.
import { installDispatchHook } from './continue-dispatch-hook.mjs';
import { createNativeDispatcher } from './continue-native-dispatch.mjs';
import { recordBoundaries } from './native-snapshot-boundary.mjs';

const { ASB_CONTINUE_BUNDLE: bundle, ASB_CONTINUE_MODE: mode,
  ASB_CONTINUE_SOCKET: socket, ASB_CONTINUE_MARKER: marker } = process.env;
if (!bundle || !socket || !['record', 'replay'].includes(mode)) throw new Error('Continue native bootstrap configuration is incomplete');
installDispatchHook(bundle, createNativeDispatcher(mode, socket, {
  isDelegation: call => call.name === 'Subagent',
  ...recordBoundaries(marker),
}), { headlessSubagent: true, restoreReadState: mode === 'replay' });
