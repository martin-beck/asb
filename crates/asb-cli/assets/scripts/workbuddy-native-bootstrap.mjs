import {registerHooks} from 'node:module';
import {realpathSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import {createNativeDispatcher} from './native-tool-dispatch.mjs';
import {recordBoundaries} from './native-snapshot-boundary.mjs';
import {transformWorkBuddyDispatch} from './workbuddy-native-transform.mjs';

const {ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:mode,ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:marker}=process.env;
if (!bundle || !socket || !marker || !['record','replay'].includes(mode)) throw new Error('WorkBuddy native bootstrap configuration is incomplete');
const expected=pathToFileURL(realpathSync(bundle)).href;
const dispatch=createNativeDispatcher(mode,socket,{
  isDelegation: call => ['task','agent','subagent'].includes(call.name.toLowerCase()),
  toRequest: call => ({tool:call.name,input:call.input}),
  ...recordBoundaries(marker,'workbuddy'), snapshotOrder:true,
});
globalThis[Symbol.for('asb.workbuddy.nativeDispatch.v1')]=(call,invoke)=>dispatch(call,null,invoke);
registerHooks({load(url,context,nextLoad) {
  const loaded=nextLoad(url,context);
  if (url !== expected) return loaded;
  if (loaded.source == null) throw new Error('WorkBuddy native tool manager source is unavailable');
  return {...loaded,source:transformWorkBuddyDispatch(loaded.source.toString())};
}});
