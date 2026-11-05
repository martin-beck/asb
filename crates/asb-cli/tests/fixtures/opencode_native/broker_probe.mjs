import assert from 'node:assert/strict';
import {mkdtemp, writeFile, rm} from 'node:fs/promises';
import {spawn} from 'node:child_process';
import {pathToFileURL} from 'node:url';
import {join,resolve} from 'node:path';
import {NativeToolBroker} from '../../../assets/scripts/native-tool-broker.mjs';
const base=process.env.ASB_OPENCODE_PROBE_DIR;
if (!base || !process.env.ASB_OPENCODE_BINARY) throw new Error('Set ASB_OPENCODE_PROBE_DIR and ASB_OPENCODE_BINARY');
const root=await mkdtemp(join(base,'opencode-broker-')), socket=join(root,'broker.sock');
const request={tool:'bash',input:{command:'touch SHOULD_NOT_EXIST',timeout:1234}};
const events=[{ordinal:0,request,outcome:{ok:true,value:{title:'fixture',output:'intercepted',metadata:{}}}}];
const effects=[];
const broker=new NativeToolBroker('replay',events,async event=>effects.push(event.ordinal));
await broker.listen(socket);
const module=pathToFileURL(resolve('crates/asb-cli/assets/scripts/opencode-native-plugin.mjs')).href;
const plugin=join(root,'plugin.mjs');
const definitions={bash:{description:'fixture',schema:{type:'object',properties:{command:{type:'string'},timeout:{type:'integer'}},required:['command']}}};
await writeFile(plugin,`import {createOpenCodeNativePlugin} from ${JSON.stringify(module)}; export default async () => createOpenCodeNativePlugin(${JSON.stringify({mode:'replay',socket,definitions})});\n`);
const child=spawn('python3',['crates/asb-cli/tests/fixtures/opencode_native/tool_override.py'],{stdio:'inherit',env:{...process.env,ASB_OPENCODE_PROBE_PLUGIN:plugin}});
const code=await new Promise((resolve,reject)=>{child.on('error',reject);child.on('exit',resolve)});
try { assert.equal(code,0); await broker.finish(); assert.deepEqual(effects,[0]); console.log('installed OpenCode replayed broker result and did not execute native command'); }
catch(error) { await broker.abort().catch(()=>{}); throw error; }
