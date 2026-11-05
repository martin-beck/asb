import {registerHooks} from 'node:module';
import {realpathSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import {createNativeDispatcher} from './native-tool-dispatch.mjs';
import {recordBoundaries} from './native-snapshot-boundary.mjs';
import {transformWorkBuddyDispatch} from './workbuddy-native-transform.mjs';
import {installWorkBuddyToolContext} from './workbuddy-native-context.mjs';

const {ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:mode,ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:marker}=process.env;
if (!bundle || !socket || !marker || !['record','replay'].includes(mode)) throw new Error('WorkBuddy native bootstrap configuration is incomplete');
const expected=pathToFileURL(realpathSync(bundle)).href;
const providerBase=process.env.CODEBUDDY_BASE_URL;
const toolContext=mode === 'record' && providerBase ? installWorkBuddyToolContext(providerBase) : undefined;
if (toolContext) globalThis.fetch=toolContext.wrapFetch(globalThis.fetch);
const dispatch=createNativeDispatcher(mode,socket,{
  isDelegation: call => ['task','agent','subagent'].includes(call.name.toLowerCase()),
  toRequest: call => ({tool:call.name,input:call.input}),
  ...recordBoundaries(marker,'workbuddy'), snapshotOrder:true,
  ...(toolContext ? {invokeRecorded:toolContext.invokeRecorded} : {}),
  ...(mode === 'replay' ? {beforeReplay:async ordinal => {
    if (!providerBase) throw new Error('WorkBuddy replay provider endpoint is missing');
    const response=await fetch(`${providerBase.replace(/\/+$/,'')}/__asb_native_tool_consumed`,{
      method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({agent:'workbuddy',ordinal}),
    });
    if (!response.ok) throw new Error(`WorkBuddy replay could not settle nested provider exchanges (HTTP ${response.status})`);
  }} : {}),
});
globalThis[Symbol.for('asb.workbuddy.nativeDispatch.v1')]=(call,invoke)=>dispatch(call,null,invoke);
registerHooks({load(url,context,nextLoad) {
  const loaded=nextLoad(url,context);
  if (url !== expected) return loaded;
  if (loaded.source == null) throw new Error('WorkBuddy native tool manager source is unavailable');
  return {...loaded,source:transformWorkBuddyDispatch(loaded.source.toString())};
}});
