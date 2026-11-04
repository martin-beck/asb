// Continue's pinned bundle keeps its built-in tool dispatch private. Identify
// that one call structurally and wrap it without changing the tool registry,
// arguments, return value, or error behavior. This adapter-owned transform is
// only suitable for the pinned client; reject unknown or ambiguous layouts.
export function transformDispatch(source) {
  if (typeof source !== 'string') throw new TypeError('Continue bundle must be text');
  const pattern = /await\s+([A-Za-z_$][\w$]*)\.tool\.run\(\1\.preprocessResult\?\.args\?\?\1\.arguments,([A-Za-z_$][\w$]*)\)/g;
  const matches = [...source.matchAll(pattern)];
  if (matches.length !== 1) {
    throw new Error(`Continue native tool dispatch: expected one supported call, found ${matches.length}`);
  }
  const [call, request, context] = matches[0];
  const invocation = `${request}.tool.run(${request}.preprocessResult?.args??${request}.arguments,${context})`;
  const replacement = `await globalThis[Symbol.for('asb.continue.nativeDispatch.v1')](${request},${context},()=>${invocation})`;
  return source.slice(0, matches[0].index) + replacement + source.slice(matches[0].index + call.length);
}

// Continue 1.5.47 leaves its interactive chat-history session unset in
// headless mode, even after --resume. Its own session manager already owns
// the current session; use that identity for the beta Subagent tool.
export function transformHeadlessSubagent(source) {
  if (typeof source !== 'string') throw new TypeError('Continue bundle must be text');
  const original = 'if(!a)throw new Error("No active session found")';
  const first = source.indexOf(original);
  if (first < 0 || source.indexOf(original, first + original.length) >= 0 ||
      !source.includes('cE.getInstance().getCurrentSession()')) {
    throw new Error('Continue headless subagent guard is unsupported or ambiguous');
  }
  return source.slice(0, first) + 'if(!a)a=cE.getInstance().getCurrentSession().sessionId' + source.slice(first + original.length);
}
