// Shared native exchange routing; adapters define request and delegation semantics.
import { brokerRequest } from './native-tool-broker.mjs';
import { encodeThrown } from './native-tool-session.mjs';

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
      let outcome;
      try {
        outcome = error instanceof Error ? { ok: false, name: error.name, message: error.message } : { ok: false, thrown: encodeThrown(error) };
      } catch {
        await brokerRequest(socket, { op: 'invalidate', token }).catch(() => {});
        throw error;
      }
      await boundary(end);
      await brokerRequest(socket, { op: 'finish', token, outcome });
      throw error;
    }
    await boundary(end);
    let outcome;
    try {
      outcome = JSON.parse(JSON.stringify({ ok: true, value: encodeResult(value) }));
      if (!Object.hasOwn(outcome, 'value')) throw new TypeError('Native tool result is not representable as JSON');
    } catch (error) {
      // Do not leave an open exchange when a result cannot be represented.
      // The recording is invalid, but the caller still sees its original error.
      await brokerRequest(socket, { op: 'invalidate', token }).catch(() => {});
      throw error;
    }
    await brokerRequest(socket, { op: 'finish', token, outcome });
    return value;
  };
}
