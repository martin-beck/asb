import { createServer } from 'node:http';
import { createServer as createSocketServer } from 'node:net';
import { spawn } from 'node:child_process';
import { readFile, writeFile, mkdir, rm, copyFile, mkdtemp } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { ContinueBroker } from '../assets/scripts/continue-native-broker.mjs';
import { encodeSidecar, decodeSidecar } from '../assets/scripts/continue-native-sidecar.mjs';
import { replayEffects } from '../assets/scripts/continue-snapshot-boundary.mjs';
const base=resolve(process.env.ASB_TEST_PROBE_ROOT || 'target/continue-installed-probes'), cli=process.env.ASB_TEST_CONTINUE_CLI;
if(!cli)throw new Error('ASB_TEST_CONTINUE_CLI required');
await mkdir(base,{recursive:true});const root=await mkdtemp(resolve(base,'run-'));
const bundle=resolve(cli,'../index.js');const hook=new URL('../assets/scripts/continue-dispatch-hook.mjs',import.meta.url).href,dispatch=new URL('../assets/scripts/continue-native-dispatch.mjs',import.meta.url).href,boundary=new URL('../assets/scripts/continue-snapshot-boundary.mjs',import.meta.url).href;const bootPath=resolve(root,'bootstrap.mjs');await writeFile(bootPath,`import {installDispatchHook} from ${JSON.stringify(hook)};import {createNativeDispatcher} from ${JSON.stringify(dispatch)};import {recordBoundaries} from ${JSON.stringify(boundary)};installDispatchHook(process.env.ASB_TEST_BUNDLE,createNativeDispatcher(process.env.ASB_TEST_MODE,process.env.ASB_TEST_SOCKET,{isDelegation:call=>call.name==='Subagent',...recordBoundaries(process.env.ASB_TEST_MARKER)}),{headlessSubagent:true});`);const bootstrap=pathToFileURL(bootPath).href;
const markerPath=resolve(root,'marker.sock'),markers=[];
const marker=createSocketServer(client=>{let data='';client.on('data',chunk=>{data+=chunk;if(data.includes('\n')){markers.push(data.trim());client.end('1\n');}});});
await new Promise((resolve,reject)=>{marker.once('error',reject);marker.listen(markerPath,resolve);});
const tapeScript=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const programSha256=sha(await readFile(bundle));
let posts=0,gets=0,modelCalls={};
const server=createServer((req,res)=>{
 if(req.method==='GET'){gets++;res.writeHead(200,{'content-type':'text/plain'});res.end('fixture fetch result');return;}
 let data='';req.on('data',c=>data+=c);req.on('end',()=>{posts++;const body=JSON.parse(data);const child=body.model==='fixture-child', key=child?'child':'parent';modelCalls[key]=(modelCalls[key]||0)+1;const tool=modelCalls[key]===1;
 const call=child?{name:'Fetch',arguments:JSON.stringify({url:`http://127.0.0.1:${server.address().port}/fixture`})}:{name:'Subagent',arguments:JSON.stringify({description:'fixture task',prompt:'Use Fetch on the fixture URL and return the result',subagent_name:'worker'})};
 const delta=tool?{tool_calls:[{index:0,id:child?'call_fetch':'call_subagent',type:'function',function:call}]}:{content:'{"result":"done"}'};
 const event={id:'chatcmpl-fixture',object:'chat.completion.chunk',created:1,model:body.model,choices:[{index:0,delta,finish_reason:tool?'tool_calls':'stop'}]};res.writeHead(200,{'content-type':'text/event-stream'});res.end(`data: ${JSON.stringify(event)}\n\ndata: [DONE]\n\n`);
 });
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));const mockPort=server.address().port;
// The local fixture uses a Fetch with no workspace mutation. This marker is
// fixture-only and must never be treated as production snapshot evidence.
const workspaceEffectSha256=sha('local-mock-fetch-no-workspace-effects');
const cassette=resolve(root,'fixture-llm.jsonl');await rm(cassette,{force:true});
async function startProxy(mode,index,workspace) {
 const ready=resolve(root,`ready-${mode}-${index}.json`);await rm(ready,{force:true});if(mode==='replay')await rm(resolve(root,`mismatch-${index}.json`),{force:true});
 const selected=mode==='record'?cassette:resolve(root,`fixture-llm-replay-${index}.jsonl`);if(mode==='replay'){await copyFile(cassette,selected);await rm(`${selected}.progress.json`,{force:true});}
 const args=mode==='record'?['record','--port','0','--upstream',`http://127.0.0.1:${mockPort}/v1`,'--out',selected]:['replay','--port','0','--cassette',selected];
 const child=spawn(process.execPath,[tapeScript,...args],{env:{PATH:process.env.PATH||'',ASB_TAPE_READY_FILE:ready,ASB_TAPE_WORKSPACE:workspace,...(mode==='replay'?{ASB_TAPE_MISMATCH_EVIDENCE:resolve(root,`mismatch-${index}.json`)}:{})},stdio:['ignore','ignore','pipe']});let errors='',spawnError;child.stderr.on('data',c=>errors+=c);child.once('error',error=>{spawnError=error});
 try {for(let i=0;i<200;i++){if(spawnError)throw spawnError;if(child.exitCode!==null||child.signalCode!==null)throw new Error(`proxy failed: ${errors.slice(-1000)}`);try{const data=JSON.parse(await readFile(ready,'utf8'));return {child,port:data.port,selected};}catch(error){if(error.code!=='ENOENT')throw error;}await new Promise(r=>setTimeout(r,25));}throw new Error('proxy readiness timed out');}catch(error){await stop(child);throw error;}
}
async function stop(child){if(child.pid==null||child.exitCode!==null||child.signalCode!==null)return;const exited=new Promise(resolve=>child.once('exit',resolve));child.kill('SIGTERM');const timer=setTimeout(()=>child.kill('SIGKILL'),5000);try{await exited;}finally{clearTimeout(timer);}}
async function run(mode,index,events=[]) {
 const dir=resolve(root,`${mode}-${index}`);await mkdir(resolve(dir,'home'),{recursive:true});const socket=resolve(dir,'broker.sock');await rm(socket,{force:true});
 const broker=new ContinueBroker(mode,events,replayEffects(markerPath));await broker.listen(socket);
 let proxy;
 try {
  proxy=await startProxy(mode,index,dir);
  const config=resolve(dir,'config.yaml'), base=`http://127.0.0.1:${proxy.port}/v1`;
  await writeFile(config,`name: ASB fixture\nversion: 1.0.0\nmodels:\n  - name: fixture\n    provider: openai\n    model: fixture-parent\n    roles: [chat]\n    apiBase: ${base}\n    apiKey: fixture\n  - name: worker\n    provider: openai\n    model: fixture-child\n    roles: [subagent]\n    apiBase: ${base}\n    apiKey: fixture\n    chatOptions:\n      baseSystemMessage: You are a fixture helper.\n`);
  const env={PATH:process.env.PATH||'',NODE_OPTIONS:`--import=${bootstrap}`,ASB_TEST_BUNDLE:bundle,ASB_TEST_MODE:mode,ASB_TEST_SOCKET:socket,ASB_TEST_MARKER:markerPath,CONTINUE_GLOBAL_DIR:resolve(dir,'home')};
  const child=spawn(process.execPath,[cli,'-p','Delegate the fixture task to the worker subagent and finish.','--config',config,'--model','fixture','--format','json','--auto','--beta-subagent-tool'],{cwd:dir,env,stdio:['ignore','pipe','pipe']});
  let out='',err='';child.stdout.on('data',c=>out+=c);child.stderr.on('data',c=>err+=c);
  const timer=setTimeout(()=>child.kill('SIGKILL'),60000);
  let code;try {code=await new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',resolve)});} finally {clearTimeout(timer);}
  if(code!==0)throw new Error(`${mode} ${index} client exited ${code}: ${err.slice(-1200)}`);
  const result=await broker.finish();
  if(mode==='replay'){const progress=JSON.parse(await readFile(`${proxy.selected}.progress.json`,'utf8'));if(progress.consumed!==4||progress.total!==4||progress.rejected!==0)throw new Error(`replay ${index} incomplete: ${JSON.stringify(progress)}`);}
  return {result,outputBytes:Buffer.byteLength(out)};
 } catch(error){await broker.abort().catch(()=>{});throw error;}
 finally {if(proxy)await stop(proxy.child);}
}

try {
 const record=await run('record',0);if(record.result.length!==1||gets!==1||posts!==4)throw new Error(`unexpected record exchanges events=${record.result.length} gets=${gets} posts=${posts}`);
 const sidecar=encodeSidecar(record.result,programSha256,workspaceEffectSha256);await writeFile(resolve(root,'fixture-native.jsonl'),sidecar);
 const sealed=decodeSidecar(sidecar);await new Promise(resolve=>server.close(resolve));
 const settled=await Promise.allSettled([0,1,2,3].map(i=>run('replay',i,sealed.exchanges)));const failures=settled.filter(x=>x.status==='rejected');if(failures.length)throw new Error(`replay failures: ${failures.map(x=>x.reason.message).join('; ')}`);const replay=settled.map(x=>x.value);
 if(gets!==1||posts!==4)throw new Error('replay contacted the stopped mock backend');
 if(markers.filter(x=>/^tool-begin:continue-0:\d+:\d+$/.test(x)).length!==1||markers.filter(x=>/^tool-end:continue-0:\d+:\d+$/.test(x)).length!==1||markers.filter(x=>x==='tool-end:continue-0').length!==4)throw new Error(`unexpected snapshot markers: ${JSON.stringify(markers)}`);
 const summary={recordEvents:record.result.length,recordMockPosts:posts,recordMockGets:gets,replayReplicas:replay.length,replayMockPosts:0,replayMockGets:0,snapshotMarkers:markers,replayOutputBytes:replay.map(x=>x.outputBytes),artifactDirectory:root,scope:'local-mock Fetch with simulated snapshot service; not ASB, Btrfs, or SWE-bench'};await writeFile(resolve(root,'summary.json'),JSON.stringify(summary,null,2)+'\n');console.log(JSON.stringify(summary));
} finally {if(server.listening)await new Promise(resolve=>server.close(resolve));if(marker.listening)await new Promise(resolve=>marker.close(resolve));}
