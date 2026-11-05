import {createServer as httpServer} from 'node:http';
import {createServer as netServer} from 'node:net';
import {spawn} from 'node:child_process';
import {mkdir,writeFile,mkdtemp,readFile,rm} from 'node:fs/promises';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {NativeToolBroker} from '../../../assets/scripts/native-tool-broker.mjs';
import {snapshotBoundary} from '../../../assets/scripts/native-snapshot-boundary.mjs';
if (!process.env.ASB_CLAUDE_FIXTURE_DIR || !process.env.ASB_CLAUDE_BINARY) throw new Error('Set ASB_CLAUDE_FIXTURE_DIR and ASB_CLAUDE_BINARY');
const root=await mkdtemp(join(process.env.ASB_CLAUDE_FIXTURE_DIR,'edit-'));const workspace=process.env.ASB_CLAUDE_FIXED_WORKSPACE || join(root,'workspace');if (!process.env.ASB_CLAUDE_FIXED_WORKSPACE) await mkdir(workspace);const file=join(workspace,'fixture.txt');
const binary=process.env.ASB_CLAUDE_BINARY;
const modulePath=fileURLToPath(new URL('../../../assets/scripts/claude-native-hook.mjs',import.meta.url));
const hook=JSON.stringify(process.execPath)+' --input-type=module -e '+JSON.stringify(`import(${JSON.stringify(modulePath)}).then(m=>m.main()).catch(e=>{console.error(e.message);process.exitCode=1})`);
const settings={hooks:Object.fromEntries(['PreToolUse','PostToolUse','PostToolUseFailure'].map(n=>[n,[{matcher:'*',hooks:[{type:'command',command:hook,onFailure:'block',timeout:30}]}]]))};
let observed=[];const server=httpServer((req,res)=>{let body='';req.on('data',x=>body+=x);req.on('end',()=>{if(!body){res.writeHead(404);return res.end()}const d=JSON.parse(body);const results=[];for(const m of d.messages||[])if(Array.isArray(m.content))for(const x of m.content)if(x.type==='tool_result')results.push(x);observed=results;
const operations=[];
if(process.env.ASB_CLAUDE_TEST_TMP){
 const path=join(process.env.ASB_CLAUDE_TEST_TMP,'shell-output');
 const counter=process.env.ASB_CLAUDE_TEST_COUNTER;
 const quote=value=>`'${value.replaceAll("'", "'\\''")}'`;
 operations.push({type:'tool_use',id:'toolu_shell',name:'Bash',input:{command:`printf fixture > ${quote(path)}; printf x >> ${quote(counter)}`}});
 operations.push({type:'tool_use',id:'toolu_temporary',name:'Read',input:{file_path:path}});
}
operations.push({type:'tool_use',id:'toolu_read',name:'Read',input:{file_path:file}});
operations.push({type:'tool_use',id:'toolu_edit',name:'Edit',input:{file_path:file,old_string:'before',new_string:'after'}});
const content=results.length<operations.length?[operations[results.length]]:[{type:'text',text:'done'}];res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify({id:'msg_fixture',type:'message',role:'assistant',model:'claude-sonnet-4-5',content,stop_reason:results.length<operations.length?'tool_use':'end_turn',stop_sequence:null,usage:{input_tokens:10,output_tokens:10}}))})});await new Promise(r=>server.listen(0,'127.0.0.1',r));
async function run(name,events){await writeFile(file,'before\n');const dir=join(root,name);await mkdir(dir);await mkdir(join(dir,'config'));await writeFile(join(dir,'settings.json'),JSON.stringify(settings));const socket=join(dir,'broker.sock'),marker=process.env.ASB_CLAUDE_EXTERNAL_MARKER || join(dir,'marker.sock');const markerServer=process.env.ASB_CLAUDE_EXTERNAL_MARKER ? null : netServer(c=>{let data='';c.on('data',x=>{data+=x;if(data.includes('\n'))c.end('1\n')})});if(markerServer)await new Promise(r=>markerServer.listen(marker,r));const effects=[];const broker=new NativeToolBroker(events?'replay':'record',events||[],async e=>{if(process.env.ASB_CLAUDE_EXTERNAL_MARKER)await snapshotBoundary(marker,'replay','end',e.ordinal,'claude-code');effects.push(e.ordinal)});await broker.listen(socket);
const env={...process.env,CLAUDE_CONFIG_DIR:join(dir,'config'),ANTHROPIC_BASE_URL:`http://127.0.0.1:${server.address().port}`,ANTHROPIC_AUTH_TOKEN:'fixture',ANTHROPIC_API_KEY:'',ANTHROPIC_DEFAULT_SONNET_MODEL:'claude-sonnet-4-5',ASB_CLAUDE_NATIVE_DIRECTORY:dir,ASB_CLAUDE_NATIVE_SOCKET:socket,ASB_CLAUDE_NATIVE_MARKER:marker,ASB_CLAUDE_NATIVE_MODE:events?'replay':'record'};
const child=spawn(binary,['-p','--no-session-persistence','--output-format','json','--model','sonnet','--dangerously-skip-permissions','--settings',join(dir,'settings.json'),'Use Read and Edit.'],{cwd:workspace,env,stdio:['ignore','pipe','pipe']});let out='',err='';child.stdout.on('data',x=>out+=x);child.stderr.on('data',x=>err+=x);const code=await new Promise(r=>child.on('exit',r));let recorded;try{recorded=await broker.finish()}finally{if(markerServer)await new Promise(r=>markerServer.close(r))}return {code,output:JSON.parse(out),err,recorded,effects,file:await readFile(file,'utf8'),results:structuredClone(observed)};}
try {
  const phase=process.env.ASB_CLAUDE_PHASE;
  const cassette=process.env.ASB_CLAUDE_EVENTS;
  if (phase && (!['record','replay'].includes(phase) || !cassette || !process.env.ASB_CLAUDE_FIXED_WORKSPACE))
    throw new Error('Phased fixture requires a phase, cassette path, and fixed workspace');
  if (phase === 'record') {
    const record=await run('record');
    if (record.code!==0 || record.file!=='after\n' || record.recorded.length!==(process.env.ASB_CLAUDE_TEST_TMP ? 4 : 2)) throw new Error('fixture recording failed');
    await writeFile(cassette,JSON.stringify({events:record.recorded,results:record.results}),{flag:'wx'});
    console.log('record passed');
  } else if (phase === 'replay') {
    const recorded=JSON.parse(await readFile(cassette,'utf8'));
    const replay=await run('replay',recorded.events);
    if (replay.code!==0 || replay.file!=='after\n' || replay.effects.length!==(process.env.ASB_CLAUDE_TEST_TMP ? 4 : 2) || JSON.stringify(recorded.results)!==JSON.stringify(replay.results))
      throw new Error('fixture replay differs from the recording');
    console.log('replay passed');
  } else {
    const record=await run('record');const replay=await run('replay',record.recorded);
    const ok=record.code===0&&replay.code===0&&record.file==='after\n'&&replay.file==='after\n'&&replay.effects.length===2&&JSON.stringify(record.results)===JSON.stringify(replay.results);
    console.log(JSON.stringify({ok,tools:record.recorded.map(e=>e.request.tool),replayEffects:replay.effects}));if(!ok)process.exitCode=1;
  }
} finally { await new Promise(r=>server.close(r));await rm(root,{recursive:true,force:true}); }
