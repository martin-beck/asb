// Opt-in installed Qwen test: nested web-summary LLM exchange and four offline replays.
import assert from 'node:assert/strict';
import {createServer as httpServer} from 'node:http';
import {createServer as netServer} from 'node:net';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync} from 'node:fs';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {NativeToolBroker} from '../assets/scripts/native-tool-broker.mjs';
import {replayEffects} from '../assets/scripts/native-snapshot-boundary.mjs';

const {ASB_TEST_QWEN_CLI:cli,ASB_TEST_QWEN_BUNDLE:bundle,ASB_TEST_QWEN_PROBE_DIR:base}=process.env;
if(!cli||!bundle||!base)throw new Error('Set the Qwen CLI, bundle and probe directory');
const root=mkdtempSync(join(base,'qwen-web-replay-')), marker=join(root,'marker.sock'), cassette=join(root,'cassette.jsonl');
const workspace=join(root,'workspace');mkdirSync(workspace);
const agentHome=join(root,'home');mkdirSync(agentHome);
let sequence=1, pageRequests=0, providerRequests=0;
const markerServer=netServer(client=>client.once('data',data=>{if(data.toString().startsWith('tool-end:'))sequence++;client.end(`${sequence}\n`)}));
await new Promise(resolve=>markerServer.listen(marker,resolve));
const provider=httpServer(async(request,response)=>{
 if(request.method==='GET'){pageRequests++;response.writeHead(200,{'content-type':'text/html'});response.end(`<html><body>${'A fixture sentence about a tree. '.repeat(600)}</body></html>`);return;}
 providerRequests++;
 let raw='';for await(const chunk of request)raw+=chunk;const input=JSON.parse(raw),nested=!input.tools?.length;
 const done=(input.messages||[]).some(message=>message.role==='tool');
 const delta=nested||done?{content:nested?'A concise fixture summary.':'done'}:{tool_calls:[{index:0,id:'call_web',type:'function',function:{name:'web_fetch',arguments:JSON.stringify({url:`http://127.0.0.1:${provider.address().port}/page`,prompt:'Summarize the page.'})}}]};
 response.writeHead(200,{'content-type':'text/event-stream'});
 for(const choice of [{index:0,delta,finish_reason:null},{index:0,delta:{},finish_reason:nested||done?'stop':'tool_calls'}])response.write(`data: ${JSON.stringify({id:'chatcmpl-fixture',object:'chat.completion.chunk',created:1,model:'fixture',choices:[choice]})}\n\n`);
 response.end('data: [DONE]\n\n');
});
await new Promise(resolve=>provider.listen(0,'127.0.0.1',resolve));
const bootstrap=fileURLToPath(new URL('../assets/scripts/qwen-native-bootstrap.mjs',import.meta.url));
const proxyScript=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
async function execute(mode,index,events=[]){
 const home=join(root,`${mode}-${index}`),socket=join(root,`${mode}-${index}.sock`),ready=join(home,'ready.json');mkdirSync(home);
 const selected=mode==='record'?cassette:join(home,'cassette.jsonl');if(mode==='replay')copyFileSync(cassette,selected);
 const proxy=spawn(process.execPath,[proxyScript,mode,'--port','0',...(mode==='record'?['--upstream',`http://127.0.0.1:${provider.address().port}`,'--out',selected]:['--cassette',selected])],{env:{...process.env,ASB_TAPE_READY_FILE:ready,...(mode==='replay'?{ASB_TAPE_MISMATCH_EVIDENCE:join(home,'mismatch.json')}: {})},stdio:'ignore'});
 for(let n=0;n<500&&!existsSync(ready);n++)await new Promise(r=>setTimeout(r,10));
 assert.ok(existsSync(ready),'proxy failed to start');const port=JSON.parse(readFileSync(ready)).port;
 const broker=new NativeToolBroker(mode,events,replayEffects(marker,'qwen-code'));await broker.listen(socket);
 const env={PATH:process.env.PATH,HOME:agentHome,TERM:'dumb',NO_COLOR:'1',QWEN_DEFAULT_AUTH_TYPE:'openai',OPENAI_API_KEY:'fixture',OPENAI_MODEL:'fixture',OPENAI_BASE_URL:`http://127.0.0.1:${port}/v1`,ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:mode,ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:marker,NODE_OPTIONS:`--import=${JSON.stringify(bootstrap)}`};
 const child=spawn(process.execPath,[cli,'--model','fixture','--output-format','json','--yolo','Fetch and summarize the page.'],{cwd:workspace,env,stdio:['ignore','ignore','pipe']});let stderr='';child.stderr.on('data',x=>stderr+=x);
 const deadline=setTimeout(()=>child.kill('SIGKILL'),60000);const [status]=await once(child,'exit');clearTimeout(deadline);
 try {assert.equal(status,0,stderr);return await broker.finish();} finally {proxy.kill('SIGTERM');await once(proxy,'exit');}
}
try {
 const events=await execute('record',0);assert.equal(events.length,1);
 const rows=readFileSync(cassette,'utf8').trim().split('\n').map(JSON.parse);
 assert.equal(rows.filter(row=>row.native_tool_context?.ordinal===0).length,1);
 const before=pageRequests, beforeProvider=providerRequests;
 await Promise.all(Array.from({length:4},(_,index)=>execute('replay',index,events)));
 assert.equal(pageRequests,before,'replay must not fetch the original page');
 assert.equal(providerRequests,beforeProvider,'replay must not contact the provider');
 console.log(`Qwen nested-provider recording and four offline replays passed; evidence: ${root}`);
} finally {await new Promise(r=>provider.close(r));await new Promise(r=>markerServer.close(r));}
