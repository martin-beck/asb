import assert from 'node:assert/strict';
import { transformQwenDispatch } from '../assets/scripts/qwen-native-transform.mjs';

const shell = 'return invocation.execute(execSignal,liveOutputCallback,shellExecutionConfig,setPidCallback,setPromoteAbortControllerCallback,canPromoteForegroundShell)';
const plain = 'return invocation.execute(execSignal,liveOutputCallback,shellExecutionConfig)';
const source = `function shell(scheduledCall,invocation,execSignal,liveOutputCallback,shellExecutionConfig,setPidCallback,setPromoteAbortControllerCallback,canPromoteForegroundShell){${shell}}\nfunction plain(scheduledCall,invocation,execSignal,liveOutputCallback,shellExecutionConfig){${plain}}\nfunction scope(scheduledCall,signal,toolSpan){return ()=>this._executeToolCallBody(scheduledCall,signal,toolSpan)}`;
const seen = [];
globalThis[Symbol.for('asb.qwen.nativeDispatch.v1')] = (call, invoke) => { seen.push(call.request.name); return invoke(); };
const { shell: runShell, plain: runPlain } = await import(`data:text/javascript,${encodeURIComponent(transformQwenDispatch(source) + ';export {shell,plain}')}`);
const invocation = { execute: (...args) => args };
assert.deepEqual(runPlain({request:{name:'read_file'}},invocation,1,2,3), [1,2,3]);
assert.deepEqual(runShell({request:{name:'run_shell_command'}},invocation,1,2,3,4,5,6), [1,2,3,4,5,6]);
assert.deepEqual(seen, ['read_file','run_shell_command']);
assert.throws(() => transformQwenDispatch(plain), /unsupported or ambiguous/);
assert.throws(() => transformQwenDispatch(source + shell), /unsupported or ambiguous/);
assert.throws(() => transformQwenDispatch(null), TypeError);
delete globalThis[Symbol.for('asb.qwen.nativeDispatch.v1')];
