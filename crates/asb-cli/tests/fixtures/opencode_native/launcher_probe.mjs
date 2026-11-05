import assert from 'node:assert/strict';
import {mkdtemp,writeFile} from 'node:fs/promises';
import {spawn} from 'node:child_process';
import {join,resolve} from 'node:path';
import {NativeToolBroker} from '../../../assets/scripts/native-tool-broker.mjs';
import {encodeSidecar} from '../../../assets/scripts/native-tool-sidecar.mjs';
const base=process.env.ASB_OPENCODE_PROBE_DIR;
if (!base || !process.env.ASB_OPENCODE_BINARY) throw new Error('Set ASB_OPENCODE_PROBE_DIR and ASB_OPENCODE_BINARY');
const root=await mkdtemp(join(base,'opencode-launcher-')),socket=join(root,'broker.sock'), cassette=join(root,'tape.jsonl'), sidecar=join(root,'native.jsonl');
const request={tool:'bash',input:{command:'touch SHOULD_NOT_EXIST',timeout:1234}};
const events=[{ordinal:0,request,outcome:{ok:true,value:{title:'fixture',output:'intercepted',metadata:{}}}}];
const definitions={type:'function',function:{name:'bash',description:'fixture',parameters:{type:'object',properties:{command:{type:'string'},timeout:{type:'integer'}},required:['command']}}};
await writeFile(cassette,JSON.stringify({bash:{description:definitions.function.description,schema:definitions.function.parameters}}));
await writeFile(sidecar,encodeSidecar(events,'a'.repeat(64),'b'.repeat(64),'opencode'));
const effects=[];const broker=new NativeToolBroker('replay',events,async event=>effects.push(event.ordinal));await broker.listen(socket);
const env={...process.env,ASB_OPENCODE_PROBE_PLUGIN:resolve('crates/asb-cli/assets/scripts/opencode-native-launcher.mjs'),ASB_OPENCODE_NATIVE_MODE:'replay',ASB_OPENCODE_SOCKET:socket,ASB_OPENCODE_EVIDENCE:cassette,ASB_OPENCODE_SIDECAR:sidecar};
const child=spawn('python3',['crates/asb-cli/tests/fixtures/opencode_native/tool_override.py'],{stdio:'inherit',env});
const code=await new Promise((resolve,reject)=>{child.on('error',reject);child.on('exit',resolve)});
try { assert.equal(code,0); await broker.finish(); assert.deepEqual(effects,[0]); console.log('installed OpenCode loaded recorded definitions and replayed broker result'); }
catch(error) { await broker.abort().catch(()=>{}); throw error; }
