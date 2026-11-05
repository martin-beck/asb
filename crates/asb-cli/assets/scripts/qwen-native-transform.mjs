// Qwen Code 0.25.1's scheduler has separate shell and non-shell dispatches.
// Wrap only those calls, leaving the tool registry and invocation arguments intact.
const calls = [
  'return invocation.execute(execSignal,liveOutputCallback,shellExecutionConfig,setPidCallback,setPromoteAbortControllerCallback,canPromoteForegroundShell)',
  'return invocation.execute(execSignal,liveOutputCallback,shellExecutionConfig)',
];

export function transformQwenProvider(source) {
  if (typeof source !== 'string') throw new TypeError('Qwen provider source must be text');
  const original = 'const fetchLike=baseFetch;';
  const index = source.indexOf(original);
  if (index < 0 || source.indexOf(original, index + original.length) >= 0) {
    throw new Error('Qwen provider fetch layout is unsupported or ambiguous');
  }
  return source.slice(0, index) + "const fetchLike=globalThis[Symbol.for('asb.qwen.providerFetch.v1')](baseFetch);" + source.slice(index + original.length);
}

export function transformQwenDispatch(source) {
  if (typeof source !== 'string') throw new TypeError('Qwen scheduler source must be text');
  const outer = '()=>this._executeToolCallBody(scheduledCall,signal,toolSpan)';
  const outerIndex = source.indexOf(outer);
  if (outerIndex < 0 || source.indexOf(outer, outerIndex + outer.length) >= 0) {
    throw new Error('Qwen native tool scope layout is unsupported or ambiguous');
  }
  source = source.slice(0, outerIndex) + "()=>globalThis[Symbol.for('asb.qwen.toolScope.v1')](scheduledCall,()=>this._executeToolCallBody(scheduledCall,signal,toolSpan))" + source.slice(outerIndex + outer.length);
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
