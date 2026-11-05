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
    wrapRequest: (original, protocol) => function(...args) {
      const token = context.getStore()?.ordinal;
      if (token === undefined) return original.apply(this, args);
      const first = args[0];
      const optionsIndex = typeof first === 'string' || first instanceof URL ? 1 : 0;
      const supplied = args[optionsIndex];
      const options = supplied && typeof supplied === 'object' && !(supplied instanceof URL) ? supplied : {};
      let url;
      try {
        url = optionsIndex === 1 ? new URL(first) : new URL(`${options.protocol || protocol}//${options.hostname || options.host}${options.port ? `:${options.port}` : ''}${options.path || '/'}`);
      } catch { return original.apply(this, args); }
      const prefix = base.pathname.replace(/\/$/, '');
      if (url.origin !== base.origin || !(url.pathname === prefix || url.pathname.startsWith(prefix + '/'))) return original.apply(this, args);
      const patched = {...options, headers:{...options.headers, 'x-asb-native-tool':`v1:workbuddy:${token}`}};
      if (optionsIndex === 1 && typeof supplied === 'function') args.splice(1,0,patched);
      else args[optionsIndex] = patched;
      return original.apply(this, args);
    },
  };
}
