// Opt-in Qwen web-fetch probe. All provider and page traffic stays on loopback.
import assert from 'node:assert/strict';
import {createServer as httpServer} from 'node:http';
import {createServer as netServer} from 'node:net';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdtempSync} from 'node:fs';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {NativeToolBroker} from '../assets/scripts/native-tool-broker.mjs';
const {ASB_TEST_QWEN_CLI:cli,ASB_TEST_QWEN_BUNDLE:bundle,ASB_TEST_QWEN_PROBE_DIR:base}=process.env;
if(!cli||!bundle||!base)throw new Error('Set the Qwen CLI, bundle and probe directory');
const root=mkdtempSync(join(base,'qwen-web-context-')),socket=join(root,'broker.sock'),marker=join(root,'marker.sock');
let sequence=1;
const markerServer=netServer(client=>client.once('data',data=>{if(data.toString().startsWith('tool-end:'))sequence++;client.end(`${sequence}\n`)}));
await new Promise(resolve=>markerServer.listen(marker,resolve));
let mainCalls=0, nestedTags=[];
const server=httpServer(async(request,response)=>{
 if(request.method==='GET'){response.writeHead(200,{'Content-Type':'text/html'});response.end(`<html><body>${'A fixture sentence about a tree. '.repeat(600)}</body></html>`);return;}
 let raw='';for await(const chunk of request)raw+=chunk;const input=JSON.parse(raw),nested=!input.tools?.length;
 if(nested)nestedTags.push({tag:request.headers['x-asb-native-tool']??null,messages:input.messages?.length,summary:JSON.stringify(input.messages?.[0]?.content||'').toLowerCase().includes('summar')}); else mainCalls++;
 const done=(input.messages||[]).some(message=>message.role==='tool');
 const text=nested?'A concise fixture summary.':'done';
 const delta=nested||done?{content:text}:{tool_calls:[{index:0,id:'call_web',type:'function',function:{name:'web_fetch',arguments:JSON.stringify({url:`http://127.0.0.1:${server.address().port}/page`,prompt:'Summarize the page.'})}}]};
 response.writeHead(200,{'Content-Type':'text/event-stream'});
 for(const choice of [{index:0,delta,finish_reason:null},{index:0,delta:{},finish_reason:nested||done?'stop':'tool_calls'}])response.write(`data: ${JSON.stringify({id:'chatcmpl-fixture',object:'chat.completion.chunk',created:1,model:'fixture',choices:[choice]})}\n\n`);
 response.end('data: [DONE]\n\n');
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const broker=new NativeToolBroker('record');await broker.listen(socket);
const bootstrap=fileURLToPath(new URL('../assets/scripts/qwen-native-bootstrap.mjs',import.meta.url));
const env={PATH:process.env.PATH,HOME:root,TERM:'dumb',NO_COLOR:'1',QWEN_DEFAULT_AUTH_TYPE:'openai',OPENAI_API_KEY:'fixture',OPENAI_MODEL:'fixture',OPENAI_BASE_URL:`http://127.0.0.1:${server.address().port}/v1`,ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:'record',ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:marker,NODE_OPTIONS:`--import=${JSON.stringify(bootstrap)}`};
const child=spawn(process.execPath,[cli,'--model','fixture','--output-format','json','--yolo','Fetch and summarize the page.'],{cwd:root,env,stdio:['ignore','pipe','pipe']});
let stderr='';child.stderr.on('data',x=>stderr+=x);child.stdout.resume();const deadline=setTimeout(()=>child.kill('SIGKILL'),45000);const [status]=await once(child,'exit');clearTimeout(deadline);
try {assert.equal(status,0,stderr);const events=await broker.finish();assert.ok(events.length>=1);assert.ok(mainCalls>=1);assert.deepEqual(nestedTags,[{tag:'v1:qwen-code:0',messages:2,summary:true}]);console.log(`Qwen web-fetch attribution passed; evidence: ${root}`);} finally {await new Promise(r=>server.close(r));await new Promise(r=>markerServer.close(r));}
