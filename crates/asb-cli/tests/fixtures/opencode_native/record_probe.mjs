import assert from 'node:assert/strict';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {spawn} from 'node:child_process';
import {pathToFileURL} from 'node:url';
import {join,resolve} from 'node:path';
import {NativeToolBroker} from '../../../assets/scripts/native-tool-broker.mjs';
const base=process.env.ASB_OPENCODE_PROBE_DIR;
if (!base || !process.env.ASB_OPENCODE_BINARY) throw new Error('Set ASB_OPENCODE_PROBE_DIR and ASB_OPENCODE_BINARY');
const root=await mkdtemp(join(base,'opencode-record-')),socket=join(root,'broker.sock');
const broker=new NativeToolBroker('record'); await broker.listen(socket);
const module=pathToFileURL(resolve('crates/asb-cli/assets/scripts/opencode-native-plugin.mjs')).href;
const plugin=join(root,'plugin.mjs');
await writeFile(plugin,`import {createOpenCodeNativePlugin} from ${JSON.stringify(module)}; export default async () => createOpenCodeNativePlugin(${JSON.stringify({mode:'record',socket})});\n`);
const failure=Boolean(process.env.ASB_OPENCODE_FAILURE_PROBE);
const extra=failure ? {ASB_OPENCODE_PROBE_TOOL:'edit', ASB_OPENCODE_PROBE_ARGS:JSON.stringify({filePath:'/no-such-file-asb-native-probe',oldString:'old',newString:'new'}), ASB_OPENCODE_PROBE_EXPECT:'no-such-file-asb-native-probe'} : {ASB_OPENCODE_PROBE_COMMAND:'printf native',ASB_OPENCODE_PROBE_EXPECT:'native'};
const child=spawn('python3',['crates/asb-cli/tests/fixtures/opencode_native/tool_override.py'],{stdio:'inherit',env:{...process.env,ASB_OPENCODE_PROBE_PLUGIN:plugin,...extra}});
const code=await new Promise((resolve,reject)=>{child.on('error',reject);child.on('exit',resolve)});
try { assert.equal(code,0); const events=await broker.finish(); assert.equal(events.length,1); if (failure) { assert.equal(events[0].request.tool,'edit'); assert.equal(events[0].outcome.ok,false); assert.match(events[0].outcome.message,/no-such-file-asb-native-probe/); } else { assert.equal(events[0].request.tool,'bash'); assert.equal(events[0].request.input.command,'printf native'); assert.match(events[0].outcome.value.output,/native/); } console.log('installed OpenCode recorded native command and result'); }
catch(error) { await broker.abort().catch(()=>{}); throw error; }
