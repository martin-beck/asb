// Pinned Pi agent-core dispatch: wrap the native execute call without changing
// the advertised tools, argument validation, event updates, or error policy.
export function transformPiDispatch(source) {
  if (typeof source !== 'string') throw new TypeError('Pi agent source must be text');
  const prefix = 'const result = await prepared.tool.execute(prepared.toolCall.id, prepared.args, signal, (partialResult) => {';
  const suffix = '\n        });\n        await Promise.all(updateEvents);';
  const start = source.indexOf(prefix);
  if (start < 0 || source.indexOf(prefix, start + prefix.length) >= 0) throw new Error('Pi native dispatch layout is unsupported or ambiguous');
  const end = source.indexOf(suffix, start + prefix.length);
  if (end < 0 || source.indexOf(suffix, end + suffix.length) >= 0) throw new Error('Pi native dispatch end is unsupported or ambiguous');
  return source.slice(0, start) + prefix.replace('await prepared.tool.execute(', "await globalThis[Symbol.for('asb.pi.nativeDispatch.v1')](prepared, () => prepared.tool.execute(") +
    source.slice(start + prefix.length, end) + suffix.replace('});', '}));') + source.slice(end + suffix.length);
}
