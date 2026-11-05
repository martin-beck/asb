import assert from 'node:assert/strict';
import {mkdtemp, writeFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {encodeSidecar} from '../assets/scripts/native-tool-sidecar.mjs';
import {recordedDefinitions} from '../assets/scripts/opencode-native-launcher.mjs';
const dir=await mkdtemp(join(tmpdir(),'asb-opencode-definitions-'));
try {
  const tape=join(dir,'tape.jsonl'), sidecar=join(dir,'native.jsonl');
  const events=[{ordinal:0,request:{tool:'bash',input:{command:'true'}},outcome:{ok:true,value:{title:'',output:'',metadata:{}}}}];
  await writeFile(sidecar,encodeSidecar(events,'a'.repeat(64),'b'.repeat(64),'opencode'));
  const tool={type:'function',function:{name:'bash',description:'shell',parameters:{type:'object',properties:{command:{type:'string'}},required:['command']}}};
  const row=body=>JSON.stringify({request:{body:JSON.stringify(body)}})+'\n';
  await writeFile(tape,row({model:'fixture'})+row({tools:[tool,{type:'function',function:{name:'unrecorded',parameters:{type:'object'}}}]}));
  assert.deepEqual(await recordedDefinitions(tape,sidecar),{bash:{description:'shell',schema:tool.function.parameters}});
  await writeFile(tape,row({tools:[]}));
  await assert.rejects(recordedDefinitions(tape,sidecar),/lacks tool definitions for: bash/);
} finally { await rm(dir,{recursive:true,force:true}); }
