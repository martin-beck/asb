import assert from 'node:assert/strict';
import {mkdtemp} from 'node:fs/promises';
import {createServer} from 'node:net';
import {spawn} from 'node:child_process';
import {join,resolve} from 'node:path';
import {NativeToolBroker} from '../../../assets/scripts/native-tool-broker.mjs';
const base=process.env.ASB_WORKBUDDY_PROBE_DIR, binary=process.env.ASB_WORKBUDDY_BINARY, bundle=process.env.ASB_WORKBUDDY_BUNDLE;
if (!base || !binary || !bundle) throw new Error('Set ASB_WORKBUDDY_PROBE_DIR, ASB_WORKBUDDY_BINARY and ASB_WORKBUDDY_BUNDLE');
const root=await mkdtemp(join(base,'workbuddy-broker-')), socket=join(root,'broker.sock'), marker=join(root,'marker.sock');
let sequence=1;const markerServer=createServer(client=>client.once('data',chunk=>client.end(`${String(chunk).startsWith('tool-end:') ? ++sequence : 1}\n`)));
await new Promise(resolve=>markerServer.listen(marker,resolve));const broker=new NativeToolBroker('record');await broker.listen(socket);
const env={...process.env,ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:'record',ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:marker,NODE_OPTIONS:`--import=${resolve('crates/asb-cli/assets/scripts/workbuddy-native-bootstrap.mjs')}`};
async function invoke(mode) {
 const child=spawn('python3',['crates/asb-cli/tests/fixtures/workbuddy_native/client.py'],{env:{...env,ASB_NATIVE_MODE:mode},stdio:['ignore','pipe','inherit']});
 let output='';child.stdout.on('data',chunk=>output+=chunk);
 const code=await new Promise((resolve,reject)=>{child.on('error',reject);child.on('exit',resolve)});
 assert.equal(code,0); const result=JSON.parse(output.trim()); console.log(JSON.stringify({mode,...result})); return result;
}
try {
 const recorded=await invoke('record');assert.equal(recorded.command_executed,true);
 const events=await broker.finish();assert.ok(events.length>0,'installed client produced no recorded native calls');
 const effects=[];const replay=new NativeToolBroker('replay',events,async event=>effects.push(event.ordinal));await replay.listen(socket);
 const replayed=await invoke('replay');assert.equal(replayed.command_executed,false);await replay.finish();
 assert.equal(effects.length,events.length);console.log(`recorded and replayed ${events.length} native calls without executing replay command`);
} finally {await new Promise(resolve=>markerServer.close(resolve));}
