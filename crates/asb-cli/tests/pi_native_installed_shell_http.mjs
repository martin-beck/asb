// Opt-in offline probe of the installed Pi core and its actual native Bash tool.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer as createHttpServer } from 'node:http';
import { createServer } from 'node:net';
import { once } from 'node:events';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';

const scripts=fileURLToPath(new URL('../assets/scripts',import.meta.url));
const {ASB_TEST_PI_BUNDLE:bundle,ASB_TEST_PI_BASH:bash}=process.env;
if (!bundle || !bash) throw new Error('Set ASB_TEST_PI_BUNDLE and ASB_TEST_PI_BASH to pinned installed Pi modules');

if (process.argv[2]==='child') {
  const {runAgentLoop}=await import(pathToFileURL(bundle));
  const {createBashTool}=await import(pathToFileURL(bash));
  const tool=createBashTool(process.env.PROBE_WORKSPACE);
  let turns=0;
  const assistant=content=>({role:'assistant',content,api:'openai-completions',provider:'fixture',model:'fixture',
    usage:{input:0,output:0,cacheRead:0,cacheWrite:0,totalTokens:0,cost:{input:0,output:0,cacheRead:0,cacheWrite:0,total:0}},
    stopReason:content.some(item=>item.type==='toolCall')?'toolUse':'stop',timestamp:1});
  const stream=async()=>{
    const message=turns++===0 ? assistant([{type:'toolCall',id:'a',name:'bash',arguments:{
      command:'curl -fsS "$PROBE_URL"; printf x >> "$PROBE_EFFECT"'}}]) : assistant([{type:'text',text:'done'}]);
    return {async *[Symbol.asyncIterator](){yield {type:'done'};},result:async()=>message};
  };
  const config={model:{provider:'fixture',id:'fixture'},toolExecution:'sequential',convertToLlm:async messages=>messages};
  let result;
  await runAgentLoop([{role:'user',content:[{type:'text',text:'fixture'}],timestamp:1}],
    {systemPrompt:'fixture',messages:[],tools:[tool]},config,
    async event=>{if(event.type==='tool_execution_end')result=event.result;},undefined,stream);
  process.stdout.write(JSON.stringify({turns,result})+'\n');
} else {
  const {NativeToolBroker}=await import(pathToFileURL(join(scripts,'native-tool-broker.mjs')));
  const root=mkdtempSync(join(tmpdir(),'asb-pi-real-shell-'));
  const markerPath=join(root,'marker.sock'),effect=join(root,'effects');
  let calls=0,upstream=createHttpServer((_request,response)=>{calls++;response.end('fixture-http');});
  await new Promise(resolve=>upstream.listen(0,'127.0.0.1',resolve));
  const url=`http://127.0.0.1:${upstream.address().port}/`;
  const marker=createServer(client=>client.once('data',data=>client.end(data.toString().startsWith('tool-begin:')?'1\n':'2\n')));
  await new Promise(resolve=>marker.listen(markerPath,resolve));
  async function run(mode,events=[],id='record') {
    const broker=new NativeToolBroker(mode,events,async()=>{});
    const socket=join(root,`broker-${id}.sock`);await broker.listen(socket);
    const env={PATH:process.env.PATH||'/usr/bin:/bin',HOME:root,ASB_TEST_PI_BUNDLE:bundle,ASB_TEST_PI_BASH:bash,
      NODE_OPTIONS:`--import=${scripts}/pi-native-bootstrap.mjs`,
      ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:mode,ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:markerPath,
      PROBE_WORKSPACE:root,PROBE_URL:url,PROBE_EFFECT:effect};
    const child=spawn('unshare',[mode==='record'?'-Ur':'-Urn',process.execPath,process.argv[1],'child'],{env,stdio:['ignore','pipe','pipe']});
    let stdout='',stderr='';child.stdout.on('data',chunk=>stdout+=chunk);child.stderr.on('data',chunk=>stderr+=chunk);
    const timeout=setTimeout(()=>child.kill('SIGKILL'),20000);
    try {
      const [code]=await once(child,'exit');assert.equal(code,0,stderr);
      return {value:JSON.parse(stdout),events:await broker.finish()};
    } catch(error) {await broker.abort().catch(()=>{});throw error;}
    finally {clearTimeout(timeout);}
  }
  try {
    const recording=await run('record');
    assert.equal(calls,1);assert.equal(readFileSync(effect,'utf8'),'x');
    assert.match(JSON.stringify(recording.value.result),/fixture-http/);
    await new Promise(resolve=>upstream.close(resolve));upstream=null;
    const replays=await Promise.all([1,2,3,4].map(id=>run('replay',recording.events,String(id))));
    for(const replay of replays) assert.deepEqual(replay.value,recording.value);
    assert.equal(calls,1);assert.equal(readFileSync(effect,'utf8'),'x','replay executed Bash');
    console.log('installed Pi native Bash: one HTTP record and four isolated replays passed');
  } finally {
    if(upstream)await new Promise(resolve=>upstream.close(resolve));
    await new Promise(resolve=>marker.close(resolve));
    rmSync(root,{recursive:true,force:true});
  }
}
