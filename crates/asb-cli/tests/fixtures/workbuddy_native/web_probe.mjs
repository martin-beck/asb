import assert from 'node:assert/strict';
import {mkdtemp} from 'node:fs/promises';
import {createServer} from 'node:net';
import {spawn} from 'node:child_process';
import {join,resolve} from 'node:path';
import {NativeToolBroker} from '../../../assets/scripts/native-tool-broker.mjs';
const base=process.env.ASB_WORKBUDDY_PROBE_DIR, binary=process.env.ASB_WORKBUDDY_BINARY, bundle=process.env.ASB_WORKBUDDY_BUNDLE;
if (!base || !binary || !bundle) throw new Error('Set probe directory, binary and bundle');
const root=await mkdtemp(join(base,'workbuddy-web-')),socket=join(root,'broker.sock'),marker=join(root,'marker.sock');let sequence=1;
const markerServer=createServer(client=>client.once('data',chunk=>client.end(`${String(chunk).startsWith('tool-end:') ? ++sequence : 1}\n`)));
await new Promise(resolve=>markerServer.listen(marker,resolve));const broker=new NativeToolBroker('record');await broker.listen(socket);
const env={...process.env,ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:'record',ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:marker,ASB_WORKBUDDY_WEB_PROBE:'1',NODE_OPTIONS:`--import=${resolve('crates/asb-cli/assets/scripts/workbuddy-native-bootstrap.mjs')}`};
const child=spawn('python3',['crates/asb-cli/tests/fixtures/workbuddy_native/client.py'],{env,stdio:['ignore','pipe','inherit']});let output='';child.stdout.on('data',chunk=>output+=chunk);
const code=await new Promise((resolve,reject)=>{child.on('error',reject);child.on('exit',resolve)});
try {assert.equal(code,0);const result=JSON.parse(output.trim());const events=await broker.finish();console.log(JSON.stringify({...result,tools:events.map(x=>x.request.tool)}));assert.ok(events.some(x=>x.request.tool==='WebFetch'));assert.ok(result.attributed>0,'WebFetch provider exchange did not carry attribution');} finally {await new Promise(resolve=>markerServer.close(resolve));}
