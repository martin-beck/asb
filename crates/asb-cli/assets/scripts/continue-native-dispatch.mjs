// Continue owns its native request and delegation semantics.
import { createNativeDispatcher as createDispatcher } from './native-tool-dispatch.mjs';
export function createNativeDispatcher(mode, socket, options = {}) {
  return createDispatcher(mode, socket, {
    ...options,
    toRequest: call => ({ tool: call.name, args: call.preprocessResult?.args ?? call.arguments }),
  });
}
