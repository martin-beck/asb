// Continue-specific dispatch bridge. An adapter supplies which calls are
// orchestration so delegated agents continue running through the same hook.
import { brokerRequest } from './continue-native-broker.mjs';

export function createNativeDispatcher(mode, socket, {
  isDelegation = () => false,
  begin = async () => {},
  end = async () => {},
} = {}) {
  if (!['record', 'replay'].includes(mode) || typeof socket !== 'string' || !socket ||
      typeof isDelegation !== 'function' || typeof begin !== 'function' || typeof end !== 'function') {
    throw new TypeError('Continue native dispatcher configuration is invalid');
  }
  return async (call, context, invoke) => {
    if (typeof invoke !== 'function') throw new TypeError('Continue native tool invocation is invalid');
    if (isDelegation(call, context)) return invoke();
    const request = { tool: call.name, args: call.preprocessResult?.args ?? call.arguments };
    if (mode === 'replay') return (await brokerRequest(socket, { op: 'invoke', request })).value;
    const { token } = await brokerRequest(socket, { op: 'start', request });
    await begin(request, token);
    let value;
    try { value = await invoke(); }
    catch (error) {
      if (!(error instanceof Error)) throw error;
      await end(request, token);
      await brokerRequest(socket, { op: 'finish', token, outcome: { ok: false, name: error.name, message: error.message } });
      throw error;
    }
    await end(request, token);
    await brokerRequest(socket, { op: 'finish', token, outcome: { ok: true, value } });
    return value;
  };
}
