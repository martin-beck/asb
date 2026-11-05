// Attribute provider requests to the native WorkBuddy tool that caused them.
import { AsyncLocalStorage } from 'node:async_hooks';

export function installWorkBuddyToolContext(providerBase) {
  const base = new URL(providerBase);
  if (!['http:', 'https:'].includes(base.protocol)) throw new TypeError('WorkBuddy provider URL is invalid');
  const context = new AsyncLocalStorage();
  return {
    scope: (_call, invoke) => context.run({ordinal: undefined}, invoke),
    invokeRecorded: (token, invoke) => {
      if (!Number.isSafeInteger(token) || token < 0 || typeof invoke !== 'function') throw new TypeError('WorkBuddy tool context is invalid');
      const state = context.getStore();
      if (state) { state.ordinal = token; return invoke(); }
      return context.run({ordinal: token}, invoke);
    },
    wrapFetch: original => function(input, init) {
      const token = context.getStore()?.ordinal;
      if (token === undefined) return original.call(this, input, init);
      const url = new URL(typeof input === 'string' || input instanceof URL ? input : input.url);
      const prefix = base.pathname.replace(/\/$/, '');
      if (url.origin !== base.origin || !(url.pathname === prefix || url.pathname.startsWith(prefix + '/'))) return original.call(this, input, init);
      const headers = new Headers(typeof input === 'object' && input !== null ? input.headers : undefined);
      new Headers(init?.headers).forEach((value, key) => headers.set(key, value));
      headers.set('x-asb-native-tool', `v1:workbuddy:${token}`);
      return original.call(this, input, {...init, headers});
    },
  };
}
