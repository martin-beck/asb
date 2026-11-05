import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, readFileSync, existsSync, unlinkSync, rmSync, openSync, closeSync } from 'node:fs';
import { createServer } from 'node:net';
import { once } from 'node:events';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { NativeToolBroker } from '../assets/scripts/native-tool-broker.mjs';
import { replayEffects } from '../assets/scripts/native-snapshot-boundary.mjs';
import { encodeSidecar, decodeSidecar } from '../assets/scripts/native-tool-sidecar.mjs';
import { createHash } from 'node:crypto';

const actualRoot = mkdtempSync(join(tmpdir(), 'asb-pi-native-'));
const directory = process.platform === 'linux' ? openSync(actualRoot,'r') : null;
const root = directory === null ? actualRoot : `/proc/${process.pid}/fd/${directory}`;
try {
  const bundle=join(root,'agent-loop.mjs'), entry=join(root,'client.mjs'), effect=join(root,'effect'), markerPath=join(root,'marker.sock'), socket=join(root,'broker.sock');
  writeFileSync(bundle,`import {writeFileSync} from 'node:fs';
async function executePreparedToolCall(prepared, signal, emit) {
    const updateEvents = [];
    try {
        const result = await prepared.tool.execute(prepared.toolCall.id, prepared.args, signal, (partialResult) => {
            updateEvents.push(Promise.resolve(emit(partialResult)));
        });
        await Promise.all(updateEvents);
        return { result, isError: false };
    } catch(error) { throw error; }
}
export const result = await executePreparedToolCall({toolCall:{id:'one',name:'bash'},args:{command:'fixture'},tool:{execute:async()=>{writeFileSync(process.env.FIXTURE_EFFECT,'live');return {content:[{type:'text',text:'done'}],details:undefined}}}},null,()=>{});
`);
  writeFileSync(entry,`import {result} from './agent-loop.mjs'; if(result.result.content[0].text!=='done'||result.result.details!==undefined)process.exit(3);`);
  const markers=[];
  const marker=createServer(client=>client.once('data',data=>{const message=data.toString().trim();markers.push(message);client.end(message.startsWith('tool-end:')?'2\n':'1\n');}));
  await new Promise(resolve=>marker.listen(markerPath,resolve));
  const bootstrap=fileURLToPath(new URL('../assets/scripts/pi-native-bootstrap.mjs',import.meta.url));
  for (const mode of ['record','replay']) {
    const missingMarker=spawn(process.execPath,[entry],{cwd:root,env:{PATH:process.env.PATH||'',NODE_OPTIONS:`--import=${JSON.stringify(bootstrap)}`,ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:mode,ASB_NATIVE_SOCKET:socket,FIXTURE_EFFECT:effect},stdio:['ignore','ignore','pipe']});
    let missingMarkerError='';missingMarker.stderr.on('data',chunk=>missingMarkerError+=chunk);
    const [missingMarkerCode]=await once(missingMarker,'exit');
    assert.notEqual(missingMarkerCode,0);
    assert.match(missingMarkerError,/requires the bundle, broker and snapshot marker/);
    assert.equal(existsSync(effect),false,'missing snapshot marker must stop the native tool');
  }
  async function run(mode,events=[]) {
    const broker=new NativeToolBroker(mode,events,replayEffects(markerPath,'pi'));await broker.listen(socket);
    const child=spawn(process.execPath,[entry],{cwd:root,env:{PATH:process.env.PATH||'',NODE_OPTIONS:`--import=${JSON.stringify(bootstrap)}`,ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:mode,ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:markerPath,FIXTURE_EFFECT:effect},stdio:['ignore','ignore','pipe']});
    let stderr='';child.stderr.on('data',chunk=>stderr+=chunk);
    const [code]=await once(child,'exit');assert.equal(code,0,stderr);
    return broker.finish();
  }
  const events=await run('record');assert.equal(events.length,1);assert.ok(existsSync(effect));unlinkSync(effect);
  const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
  const sidecar=encodeSidecar(events,hash(readFileSync(bundle)),hash('snapshot'),'pi');
  assert.equal(decodeSidecar(sidecar,'pi').exchanges.length,1);
  assert.throws(()=>decodeSidecar(sidecar,'continue'),/lifecycle/);
  await run('replay',decodeSidecar(sidecar,'pi').exchanges);
  assert.equal(existsSync(effect),false,'replay must not execute the tool');
  assert.match(markers[0],/^tool-begin:pi-0:\d+:\d+$/);
  assert.match(markers[1],/^tool-end:pi-0:\d+:\d+$/);
  assert.equal(markers[2],'tool-end:pi-0');
  await new Promise(resolve=>marker.close(resolve));
  console.log('Pi native record and replay fixture passed');
} finally { if(directory!==null)closeSync(directory);rmSync(actualRoot,{recursive:true,force:true}); }
