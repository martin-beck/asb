// Qwen Code 0.25.1's scheduler has separate shell and non-shell dispatches.
// Wrap only those calls, leaving the tool registry and invocation arguments intact.
const calls = [
  'return invocation.execute(execSignal,liveOutputCallback,shellExecutionConfig,setPidCallback,setPromoteAbortControllerCallback,canPromoteForegroundShell)',
  'return invocation.execute(execSignal,liveOutputCallback,shellExecutionConfig)',
];

export function transformQwenDispatch(source) {
  if (typeof source !== 'string') throw new TypeError('Qwen scheduler source must be text');
  const prefix = 'return globalThis[Symbol.for(\'asb.qwen.nativeDispatch.v1\')](scheduledCall,()=>';
  for (const call of calls) {
    const index = source.indexOf(call);
    if (index < 0 || source.indexOf(call, index + call.length) >= 0) {
      throw new Error('Qwen native dispatch layout is unsupported or ambiguous');
    }
    source = source.slice(0, index) + prefix + call.slice('return '.length) + ')' + source.slice(index + call.length);
  }
  return source;
}
