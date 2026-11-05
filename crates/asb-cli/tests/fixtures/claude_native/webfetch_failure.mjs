import {createServer as httpServer} from 'node:http';
import {createServer as netServer} from 'node:net';
import {spawn,spawnSync} from 'node:child_process';
import {once} from 'node:events';
import {mkdir,writeFile,readFile,mkdtemp,rm,access} from 'node:fs/promises';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {planWebFetchReplay} from '../../../assets/scripts/claude-webfetch-tape.mjs';
import {createWebFetchProxy} from '../../../assets/scripts/claude-webfetch-proxy.mjs';
import {NativeToolBroker} from '../../../assets/scripts/native-tool-broker.mjs';
if (!process.env.ASB_CLAUDE_FIXTURE_DIR || !process.env.ASB_CLAUDE_BINARY) throw new Error('Set ASB_CLAUDE_FIXTURE_DIR and ASB_CLAUDE_BINARY');
const root=await mkdtemp(join(process.env.ASB_CLAUDE_FIXTURE_DIR,'fetch-failure-'));
const workspace=join(root,'workspace');await mkdir(workspace);await writeFile(join(workspace,'fixture.txt'),'fixture text\n');
const generated=spawnSync('/usr/bin/openssl',['req','-x509','-newkey','rsa:2048','-nodes','-keyout',join(root,'key.pem'),'-out',join(root,'cert.pem'),'-days','1','-subj','/CN=example.com','-addext','subjectAltName=DNS:api.anthropic.com,DNS:example.com'],{stdio:'ignore'});if(generated.status!==0)throw new Error('certificate fixture failed');
const key=await readFile(join(root,'key.pem')), cert=await readFile(join(root,'cert.pem'));
const binary=process.env.ASB_CLAUDE_BINARY;
const hookPath=fileURLToPath(new URL('../../../assets/scripts/claude-native-hook.mjs',import.meta.url));
const hook=JSON.stringify(process.execPath)+' --input-type=module -e '+JSON.stringify(`import(${JSON.stringify(hookPath)}).then(m=>m.main()).catch(e=>{console.error(e.message);process.exitCode=1})`);
const settings={hooks:Object.fromEntries(['PreToolUse','PostToolUse','PostToolUseFailure'].map(name=>[name,[{matcher:'*',hooks:[{type:'command',command:hook,onFailure:'block',timeout:30}]}]]))};
const observed=[], summaries=[];
const http=httpServer((req,res)=>{let s='';req.on('data',b=>s+=b);req.on('end',()=>{if(!s){res.writeHead(404);return res.end()}const d=JSON.parse(s);const has=d.messages?.some(m=>Array.isArray(m.content)&&m.content.some(c=>c.type==='tool_result'));if(!d.tools?.length)summaries.push({request:{body:d}});for(const m of d.messages||[])if(Array.isArray(m.content))for(const x of m.content)if(x.type==='tool_result')observed.push(x.content);const v={id:'msg_fixture',type:'message',role:'assistant',model:'claude-sonnet-4-5',content:has?[{type:'text',text:'done'}]:!d.tools?.length?[{type:'text',text:'fixture summary'}]:[{type:'tool_use',id:'toolu_fixture',name:'WebFetch',input:{url:'https://example.com/fixture',prompt:'summarize'}}],stop_reason:has?'end_turn':!d.tools?.length?'max_tokens':'tool_use',stop_sequence:null,usage:{input_tokens:10,output_tokens:10}};res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify(v))})});await new Promise(r=>http.listen(0,'127.0.0.1',r));
async function run(name,events,plan=[{ordinal:0,url:'https://example.com/fixture',status:200,page:'<html><body>fixture content</body></html>'}]){
 const fetchProxy=createWebFetchProxy({key,cert,plan});const proxyUrl=await fetchProxy.listen();
 const dir=join(root,name);await mkdir(dir);await mkdir(join(dir,'config'));await writeFile(join(dir,'settings.json'),JSON.stringify(settings));
 const socket=join(dir,'broker.sock'),markerPath=join(dir,'marker.sock'),markers=[];
 const marker=netServer(c=>{let s='';c.on('data',x=>{s+=x;if(s.includes('\n')){markers.push(s.trim());c.end('1\n')}})});await new Promise(r=>marker.listen(markerPath,r));
 const broker=new NativeToolBroker(events?'replay':'record',events||[],async event=>{markers.push('effect:'+event.ordinal);await writeFile(join(dir,'replayed-effect'),'fixture')});await broker.listen(socket);
 const env={...process.env,CLAUDE_CONFIG_DIR:join(dir,'config'),ANTHROPIC_BASE_URL:`http://127.0.0.1:${http.address().port}`,ANTHROPIC_AUTH_TOKEN:'fixture',ANTHROPIC_API_KEY:'',ANTHROPIC_DEFAULT_SONNET_MODEL:'claude-sonnet-4-5',ASB_CLAUDE_NATIVE_DIRECTORY:dir,ASB_CLAUDE_NATIVE_SOCKET:socket,ASB_CLAUDE_NATIVE_MARKER:markerPath,ASB_CLAUDE_NATIVE_MODE:events?'replay':'record',ASB_CLAUDE_NATIVE_WEB_PROXY:proxyUrl,HTTPS_PROXY:proxyUrl,HTTP_PROXY:proxyUrl,https_proxy:proxyUrl,http_proxy:proxyUrl,NO_PROXY:'127.0.0.1,localhost',no_proxy:'127.0.0.1,localhost',NODE_EXTRA_CA_CERTS:join(root,'cert.pem'),SSL_CERT_FILE:join(root,'cert.pem')};
 const child=spawn(binary,['-p','--no-session-persistence','--output-format','json','--model','sonnet','--dangerously-skip-permissions','--settings',join(dir,'settings.json'),'Use WebFetch.'],{cwd:workspace,env,stdio:['ignore','pipe','pipe']});let out='',err='';child.stdout.on('data',x=>out+=x);child.stderr.on('data',x=>err+=x);const code=await new Promise(r=>child.on('exit',r));
 let recorded;try{recorded=await broker.finish();await fetchProxy.finish()}finally{await fetchProxy.abort();await new Promise(r=>marker.close(r))};
 const exists=async path=>access(path).then(()=>true,()=>false);
 return {code,result:JSON.parse(out).subtype,markers,executed:await exists(join(dir,'actual-execution')),applied:await exists(join(dir,'replayed-effect')),recorded,stderr:err};
}
try {
 const record=await run('record');
 if(record.code!==0 || record.recorded.length!==1 || record.recorded[0].outcome.value.kind!=='failure') throw new Error('expected recorded WebFetch failure');
 console.log('summary requests:',summaries.length,'unique message bodies:',new Set(summaries.map(x=>JSON.stringify(x.request.body.messages))).size);
 const plan=planWebFetchReplay(record.recorded,summaries);
 const replays=await Promise.all([1,2,3,4].map(index=>run(`replay-${index}`,record.recorded,plan)));
 if(replays.some(result=>result.code!==0 || !result.applied) || !observed.length || !observed.every(value=>JSON.stringify(value)===JSON.stringify(observed[0]))) throw new Error('WebFetch failure replay differed');
 console.log('Claude recorded WebFetch summary failure and four matching replays passed');
} finally { await new Promise(r=>http.close(r));await rm(root,{recursive:true,force:true}); }
