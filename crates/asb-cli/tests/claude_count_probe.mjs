import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdir,mkdtemp,readFile,rm,writeFile} from 'node:fs/promises';
import {resolve,join} from 'node:path';
import {fileURLToPath} from 'node:url';

const scratch=resolve('target/claude-count-probe');await mkdir(scratch,{recursive:true});
const root=await mkdtemp(join(scratch,'case-'));
const tape=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
const wire=fileURLToPath(new URL('../assets/scripts/claude-native-wire.mjs',import.meta.url));
const first={model:'fixture',temperature:0,messages:[{role:'user',content:'first'}]};
const count={model:'fixture',temperature:0,messages:[{role:'user',content:'count'}]};
const last={model:'fixture',temperature:0,messages:[{role:'user',content:'last'}]};
const entry=(url,body,status=200)=>({request:{method:'POST',url,headers:{},body},response:{status,headers:{'content-type':'application/json'},body:JSON.stringify({status})}});
async function run(name,status,mode){
 const dir=join(root,name);await mkdir(dir);const cassette=join(dir,'cassette.jsonl'),ready=join(dir,'ready.json');
 await writeFile(cassette,[entry('/v1/messages',first),entry('/v1/messages/count_tokens?beta=true',count,status),entry('/v1/messages',last)].map(JSON.stringify).join('\n')+'\n');
 const child=spawn(process.execPath,[tape,'replay','--port','0','--cassette',cassette],{env:{...process.env,ASB_TAPE_READY_FILE:ready,ASB_TAPE_AGENT_WIRE_EXTENSION:wire,ASB_TAPE_MISMATCH_EVIDENCE:join(dir,'mismatch.json')},stdio:['ignore','ignore','pipe']});let error='';child.stderr.on('data',d=>error+=d);
 let port;for(let i=0;i<200;i++){try{port=JSON.parse(await readFile(ready,'utf8')).port;break}catch(e){if(e.code!=='ENOENT')throw e;await new Promise(r=>setTimeout(r,25))}}
 if(!port)throw new Error(`proxy not ready: ${error}`);
 const request=async(path,body)=>fetch(`http://127.0.0.1:${port}${path}`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});
 try {
  const initial=await request('/v1/messages',first);
  if(initial.status!==200)console.error(name,await readFile(join(dir,'mismatch.json'),'utf8'));
  assert.equal(initial.status,200);
  if(mode==='requested')assert.equal((await request('/v1/messages/count_tokens?beta=true',count)).status,status);
  const output=await request('/v1/messages',mode==='changed'||status===200&&mode!=='requested'?{...last,messages:[{role:'user',content:'changed'}]}:last);
  assert.equal(output.status,mode==='changed'||status===200&&mode!=='requested'?409:200);
  if(mode==='late')assert.equal((await request('/v1/messages/count_tokens?beta=true',count)).status,404);
  const progress=JSON.parse(await readFile(`${cassette}.progress.json`,'utf8'));
  if(mode==='skip'&&status===404)assert.deepEqual([progress.consumed,progress.rejected,progress.unrequested_optional_probes],[3,0,1]);
  if(mode==='late'||mode==='requested')assert.deepEqual([progress.consumed,progress.rejected,progress.unrequested_optional_probes],[3,0,0]);
  if(mode==='changed'||status===200&&mode!=='requested')assert.equal(progress.rejected,1);
  if(status===200&&mode!=='requested')assert.equal(progress.consumed,1);
 } finally {child.kill('SIGTERM');await new Promise(r=>child.once('exit',r));}
}
try {await run('skip',404,'skip');await run('late',404,'late');await run('requested',404,'requested');await run('requested-success',200,'requested');await run('successful',200,'skip');await run('changed',404,'changed');console.log('Claude failed token-count probe: exact replay and negative cases passed');}
finally {await rm(root,{recursive:true,force:true});}
