// Adapter-local loader for the pinned Continue ESM bundle.  The caller owns
// recording/replay and installs a dispatcher before the bundle is evaluated.
import { registerHooks } from 'node:module';
import { realpathSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { transformDispatch, transformHeadlessSubagent } from './continue-dispatch-transform.mjs';

const symbol = Symbol.for('asb.continue.nativeDispatch.v1');

export function installDispatchHook(bundlePath, dispatch, { headlessSubagent = false } = {}) {
  if (typeof registerHooks !== 'function') throw new Error('Continue native dispatch requires Node.js 22.15 or newer');
  if (typeof dispatch !== 'function') throw new TypeError('Continue native dispatcher must be a function');
  if (globalThis[symbol] !== undefined) throw new Error('Continue native dispatcher is already installed');
  if (typeof headlessSubagent !== 'boolean') throw new TypeError('Continue headless subagent option must be boolean');
  const expected = pathToFileURL(realpathSync(bundlePath)).href;
  globalThis[symbol] = dispatch;
  let matched = false;
  const hooks = registerHooks({
    load(url, context, nextLoad) {
      const loaded = nextLoad(url, context);
      if (url !== expected) return loaded;
      if (loaded.source == null) throw new Error('Continue native dispatch bundle has no source');
      let source = transformDispatch(loaded.source.toString());
      if (headlessSubagent) source = transformHeadlessSubagent(source);
      matched = true;
      return { ...loaded, source };
    },
  });
  return { get matched() { return matched; }, deregister() { hooks.deregister(); delete globalThis[symbol]; } };
}
