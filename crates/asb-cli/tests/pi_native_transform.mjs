import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { transformPiDispatch } from '../assets/scripts/pi-native-transform.mjs';

const source = `async function executePreparedToolCall(prepared, signal, emit) {
    const updateEvents = [];
    try {
        const result = await prepared.tool.execute(prepared.toolCall.id, prepared.args, signal, (partialResult) => {
            updateEvents.push(Promise.resolve(emit(partialResult)));
        });
        await Promise.all(updateEvents);
        return { result, isError: false };
    } catch(error) { throw error; }
}; executePreparedToolCall`;
const context = { Symbol, Promise, globalThis: null };
context.globalThis = context;
let called = 0;
context[Symbol.for('asb.pi.nativeDispatch.v1')] = async (_prepared, invoke) => { called++; return invoke(); };
const dispatch = vm.runInNewContext(transformPiDispatch(source), context);
const result = await dispatch({ toolCall:{id:'1'},args:{a:1},tool:{execute:async (_id,args)=>args.a} }, null, ()=>{});
assert.equal(result.result,1);
assert.equal(called,1);
assert.throws(()=>transformPiDispatch('unsupported'),/unsupported/);
assert.throws(()=>transformPiDispatch(source+source),/ambiguous/);
if (process.env.ASB_TEST_PI_BUNDLE) assert.match(transformPiDispatch(readFileSync(process.env.ASB_TEST_PI_BUNDLE,'utf8')),/asb\.pi\.nativeDispatch/);
console.log('Pi native dispatch transform checks passed');
