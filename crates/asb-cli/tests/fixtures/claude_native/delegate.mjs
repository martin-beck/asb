import {createServer as httpServer} from 'node:http';
import {createServer as netServer} from 'node:net';
import {spawn} from 'node:child_process';
import {mkdir,writeFile,mkdtemp,readFile,rm,copyFile} from 'node:fs/promises';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {NativeToolBroker} from '../../../assets/scripts/native-tool-broker.mjs';
if (!process.env.ASB_CLAUDE_FIXTURE_DIR || !process.env.ASB_CLAUDE_BINARY) throw new Error('Set ASB_CLAUDE_FIXTURE_DIR and ASB_CLAUDE_BINARY');
const root=await mkdtemp(join(process.env.ASB_CLAUDE_FIXTURE_DIR,'delegate-replay-'));
const workspace=process.env.ASB_CLAUDE_FIXED_WORKSPACE || root;await writeFile(join(workspace,'fixture.txt'),'fixture child content\n');
let childRequests=0;
const http=httpServer((req,res)=>{let s='';req.on('data',b=>s+=b);req.on('end',()=>{if(!s){res.writeHead(404);return res.end()}const d=JSON.parse(s),text=JSON.stringify(d.messages||[]);let content,stop;
if(text.includes('toolu_parent')&&text.includes('tool_result')&&!text.includes('toolu_child')){content=[{type:'text',text:'parent done'}];stop='end_turn'}
else if(text.includes('child-prompt-sentinel')){childRequests++;if(text.includes('toolu_child')&&text.includes('tool_result')){content=[{type:'text',text:'child done'}];stop='end_turn'}else{content=[{type:'tool_use',id:'toolu_child',name:'Read',input:{file_path:join(workspace,'fixture.txt')}}];stop='tool_use'}}
else{content=[{type:'tool_use',id:'toolu_parent',name:'Agent',input:{description:'fixture child',prompt:'child-prompt-sentinel: read fixture file',subagent_type:'general-purpose'}}];stop='tool_use'}
res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify({id:'msg_fixture',type:'message',role:'assistant',model:'claude-sonnet-4-5',content,stop_reason:stop,stop_sequence:null,usage:{input_tokens:10,output_tokens:10}}))})});await new Promise(r=>http.listen(0,'127.0.0.1',r));
const modulePath=fileURLToPath(new URL('../../../assets/scripts/claude-native-hook.mjs',import.meta.url));
const tapePath=fileURLToPath(new URL('../../../assets/scripts/llmtape.mjs',import.meta.url));
const wirePath=fileURLToPath(new URL('../../../assets/scripts/claude-native-wire.mjs',import.meta.url));
let providerOpen=true;
async function stopProvider(){if(providerOpen){await new Promise(r=>http.close(r));providerOpen=false;}}
async function startTape(mode, cassette, name){
  const ready=join(root,`${name}-ready.json`);
  const args=[tapePath,mode,'--port','0',...(mode==='record'?['--upstream',`http://127.0.0.1:${http.address().port}`,'--out',cassette]:['--cassette',cassette])];
  const env={...process.env,ASB_TAPE_READY_FILE:ready,ASB_TAPE_WORKSPACE:workspace,ASB_TAPE_AGENT_WIRE_EXTENSION:wirePath};
  if(mode==='replay')env.ASB_TAPE_MISMATCH_EVIDENCE=join(root,'mismatch.json');
  const proxy=spawn(process.execPath,args,{env,stdio:['ignore','ignore','pipe']});
  let errors='';proxy.stderr.on('data',data=>errors+=data);
  for(let i=0;i<200;i++){
    if(proxy.exitCode!==null)throw new Error(`LLM tape exited: ${errors}`);
    try{return {proxy,url:`http://127.0.0.1:${JSON.parse(await readFile(ready,'utf8')).port}`};}
    catch(error){if(error.code!=='ENOENT')throw error;await new Promise(r=>setTimeout(r,50));}
  }
  proxy.kill('SIGTERM');throw new Error('LLM tape did not become ready');
}
async function stopTape(proxy){
  proxy.kill('SIGTERM');const code=await new Promise(r=>proxy.once('exit',r));
  if(code!==0)throw new Error('LLM tape did not settle cleanly');
}
const hook=JSON.stringify(process.execPath)+' --input-type=module -e '+JSON.stringify(`import(${JSON.stringify(modulePath)}).then(m=>m.main()).catch(e=>{console.error(e.message);process.exitCode=1})`);const settings={hooks:Object.fromEntries(['PreToolUse','PostToolUse','PostToolUseFailure'].map(name=>[name,[{matcher:'*',hooks:[{type:'command',command:hook,onFailure:'block',timeout:30}]}]]))};
async function run(name,events,endpoint){const dir=join(root,name);await mkdir(dir);const config=process.env.ASB_CLAUDE_FIXED_WORKSPACE ? join(workspace,'.claude-config') : join(dir,'config');await mkdir(config,{recursive:true});await writeFile(join(dir,'settings.json'),JSON.stringify(settings));const socket=join(dir,'broker.sock'),markerPath=join(dir,'marker.sock');const marker=netServer(c=>{let s='';c.on('data',x=>{s+=x;if(s.includes('\n'))c.end('1\n')})});await new Promise(r=>marker.listen(markerPath,r));const effects=[];const broker=new NativeToolBroker(events?'replay':'record',events||[],async e=>effects.push(e.ordinal));await broker.listen(socket);
const env={...process.env,CLAUDE_CONFIG_DIR:config,ANTHROPIC_BASE_URL:endpoint || `http://127.0.0.1:${http.address().port}`,ANTHROPIC_AUTH_TOKEN:'fixture',ANTHROPIC_API_KEY:'',ANTHROPIC_DEFAULT_SONNET_MODEL:'claude-sonnet-4-5',ASB_CLAUDE_NATIVE_DIRECTORY:dir,ASB_CLAUDE_NATIVE_SOCKET:socket,ASB_CLAUDE_NATIVE_MARKER:markerPath,ASB_CLAUDE_NATIVE_MODE:events?'replay':'record'};if(events&&endpoint)env.ASB_CLAUDE_ID_MAP_URL=`${endpoint}/.asb/claude-task-map`;const cli=process.env.ASB_CLAUDE_BINARY;const child=spawn(cli,['-p','--no-session-persistence','--output-format','json','--model','sonnet','--dangerously-skip-permissions','--settings',join(dir,'settings.json'),'Use Agent.'],{cwd:workspace,env,stdio:['ignore','pipe','pipe']});let out='',err='';child.stdout.on('data',x=>out+=x);child.stderr.on('data',x=>err+=x);const code=await new Promise(r=>child.on('exit',r));let result;try{result=await broker.finish()}finally{await new Promise(r=>marker.close(r))};return {code,subtype:JSON.parse(out).subtype,events:result,effects,err}}
let succeeded=false;
try {
  const phase=process.env.ASB_CLAUDE_PHASE,cassette=process.env.ASB_CLAUDE_EVENTS;
  if (phase && (!['record','replay'].includes(phase) || !cassette || !process.env.ASB_CLAUDE_FIXED_WORKSPACE))
    throw new Error('Phased fixture requires a phase, cassette path, and fixed workspace');
  if (phase==='record') {
    const tape=process.env.ASB_CLAUDE_STRICT_TAPE ? await startTape('record',`${cassette}.llm`,'record') : null;
    let result;try{result=await run('record',null,tape?.url)}finally{if(tape)await stopTape(tape.proxy)}
    if (result.code!==0 || !result.events.some(e=>e.request.tool==='Agent') || !result.events.some(e=>e.request.tool==='Read') || !childRequests)
      throw new Error('delegated recording failed');
    await writeFile(cassette,JSON.stringify(result.events),{flag:'wx'});
    console.log('record passed');succeeded=true;
  } else if (phase==='replay') {
    const events=JSON.parse(await readFile(cassette,'utf8'));
    let tape=null,tapeFile;
    if(process.env.ASB_CLAUDE_STRICT_TAPE){await stopProvider();tapeFile=join(root,'replay.llm');await copyFile(`${cassette}.llm`,tapeFile);tape=await startTape('replay',tapeFile,'replay');}
    let result;try{result=await run('replay',events,tape?.url)}finally{if(tape)await stopTape(tape.proxy)}
    const progress=tape ? JSON.parse(await readFile(`${tapeFile}.progress.json`,'utf8')) : null;
    if (result.code!==0 || result.effects.length!==events.length || (progress ? progress.rejected!==0 || progress.consumed!==progress.total : !childRequests))
      throw new Error('delegated replay failed');
    console.log('replay passed');succeeded=true;
  } else {
    const record=await run('record');const before=childRequests;const replay=await run('replay',record.events);
    const ok=record.code===0&&replay.code===0&&record.events.some(e=>e.request.tool==='Agent')&&record.events.some(e=>e.request.tool==='Read')&&replay.effects.length===record.events.length&&childRequests>before;
    console.log(JSON.stringify({ok,tools:record.events.map(e=>e.request.tool),ordinals:record.events.map(e=>e.ordinal),effects:replay.effects,childProviderRequests:childRequests-before}));if(!ok)process.exitCode=1;else succeeded=true;
  }
} finally { await stopProvider();if(succeeded)await rm(root,{recursive:true,force:true});else console.error(`Fixture evidence retained at ${root}`); }
