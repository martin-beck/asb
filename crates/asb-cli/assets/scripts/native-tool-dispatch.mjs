// Shared native exchange routing; adapters define request and delegation semantics.
import { brokerRequest } from './native-tool-broker.mjs';

export function createNativeDispatcher(mode, socket, {
  isDelegation = () => false,
  toRequest,
  encodeResult = value => value,
  decodeResult = value => value,
  begin = async () => {},
  end = async () => {},
} = {}) {
  if (!['record', 'replay'].includes(mode) || typeof socket !== 'string' || !socket ||
      typeof isDelegation !== 'function' || typeof toRequest !== 'function' || typeof encodeResult !== 'function' || typeof decodeResult !== 'function' || typeof begin !== 'function' || typeof end !== 'function') {
    throw new TypeError('native dispatcher configuration is invalid');
  }
  return async (call, context, invoke) => {
    if (typeof invoke !== 'function') throw new TypeError('Native tool native tool invocation is invalid');
    if (isDelegation(call, context)) return invoke();
    const request = toRequest(call, context);
    if (mode === 'replay') return decodeResult((await brokerRequest(socket, { op: 'invoke', request })).value);
    const { token } = await brokerRequest(socket, { op: 'start', request });
    // A failed boundary leaves workspace effects unaccounted for. Invalidate
    // the exchange and release its pending token before propagating failure.
    const boundary = async action => {
      try { await action(request, token); }
      catch (error) {
        await brokerRequest(socket, { op: 'invalidate', token });
        throw error;
      }
    };
    await boundary(begin);
    let value;
    try { value = await invoke(); }
    catch (error) {
      if (!(error instanceof Error)) {
        // The sidecar cannot represent arbitrary thrown values. Unblock the
        // coordinator and invalidate the recording while preserving the
        // exact value observed by the agent.
        await brokerRequest(socket, { op: 'invalidate', token });
        throw error;
      }
      await boundary(end);
      await brokerRequest(socket, { op: 'finish', token, outcome: { ok: false, name: error.name, message: error.message } });
      throw error;
    }
    await boundary(end);
    await brokerRequest(socket, { op: 'finish', token, outcome: { ok: true, value: encodeResult(value) } });
    return value;
  };
}
