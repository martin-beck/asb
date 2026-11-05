// Opt-in installed Pi core/Bash probe driven by the real snapshot supervisor.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';

const scripts=fileURLToPath(new URL('../../../assets/scripts',import.meta.url));
const {ASB_TEST_PI_BUNDLE:bundle,ASB_TEST_PI_BASH:bash,PI_FIXTURE_ROOT:root,
  PI_FIXTURE_WORKSPACE:workspace,PI_FIXTURE_TEMP:temporary,PI_FIXTURE_MARKER:marker,PI_FIXTURE_PRIVATE:privateDir,
  PI_FIXTURE_PHASE:phase}=process.env;
if (![bundle,bash,root,workspace,temporary,marker,privateDir].every(Boolean) || !['record','replay'].includes(phase)) {
  throw new Error('Pi snapshot fixture configuration is incomplete');
}
const command='curl -fsS http://127.0.0.1:29837/; printf after > "$PI_FIXTURE_WORKSPACE/fixture.txt"; printf temp > "$PI_FIXTURE_TEMP/effect"; printf x >> "$PI_FIXTURE_ROOT/counter"';
if (process.argv[2]==='child') {
  const {runAgentLoop}=await import(pathToFileURL(bundle));
  const {createBashTool}=await import(pathToFileURL(bash));
  let turns=0,result;
  const assistant=content=>({role:'assistant',content,api:'openai-completions',provider:'fixture',model:'fixture',
    usage:{input:0,output:0,cacheRead:0,cacheWrite:0,totalTokens:0,cost:{input:0,output:0,cacheRead:0,cacheWrite:0,total:0}},
    stopReason:content.some(item=>item.type==='toolCall')?'toolUse':'stop',timestamp:1});
  const stream=async()=>{
    const message=turns++===0 ? assistant([{type:'toolCall',id:'a',name:'bash',arguments:{command}}])
      : assistant([{type:'text',text:'done'}]);
    return {async *[Symbol.asyncIterator](){yield {type:'done'};},result:async()=>message};
  };
  const config={model:{provider:'fixture',id:'fixture'},toolExecution:'sequential',convertToLlm:async messages=>messages};
  await runAgentLoop([{role:'user',content:[{type:'text',text:'fixture'}],timestamp:1}],
    {systemPrompt:'fixture',messages:[],tools:[createBashTool(workspace)]},config,
    async event=>{if(event.type==='tool_execution_end')result=event.result;},undefined,stream);
  process.stdout.write(JSON.stringify({turns,result})+'\n');
} else {
  const {NativeToolBroker}=await import(pathToFileURL(join(scripts,'native-tool-broker.mjs')));
  const {replayEffects}=await import(pathToFileURL(join(scripts,'native-snapshot-boundary.mjs')));
  const eventsPath=join(root,'events.json'),resultPath=join(root,'result.json');
  const events=phase==='replay'?JSON.parse(readFileSync(eventsPath,'utf8')):[];
  const broker=new NativeToolBroker(phase,events,replayEffects(marker,'pi'));
  const socket=join(privateDir,'broker.sock');
  await broker.listen(socket);
  let requests=0,server;
  if(phase==='record') {
    server=createServer((_request,response)=>{requests++;response.end('fixture-http');});
    await new Promise(resolve=>server.listen(29837,'127.0.0.1',resolve));
  }
  const env={PATH:process.env.PATH||'/usr/bin:/bin',HOME:process.env.HOME,
    ASB_TEST_PI_BUNDLE:bundle,ASB_TEST_PI_BASH:bash,PI_FIXTURE_ROOT:root,
    PI_FIXTURE_WORKSPACE:workspace,PI_FIXTURE_TEMP:temporary,PI_FIXTURE_MARKER:marker,
    PI_FIXTURE_PHASE:phase,PI_FIXTURE_PRIVATE:privateDir,
    ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:phase,ASB_NATIVE_SOCKET:socket,
    ASB_NATIVE_MARKER:marker,NODE_OPTIONS:`--import=${scripts}/pi-native-bootstrap.mjs`};
  const child=spawn(process.execPath,[process.argv[1],'child'],{env,stdio:['ignore','pipe','pipe']});
  let stdout='',stderr='';child.stdout.on('data',chunk=>stdout+=chunk);child.stderr.on('data',chunk=>stderr+=chunk);
  const timeout=setTimeout(()=>child.kill('SIGKILL'),60000);
  try {
    const [code]=await once(child,'exit');assert.equal(code,0,stderr);
    const value=JSON.parse(stdout),recorded=await broker.finish();
    if(phase==='record') {
      assert.equal(requests,1);assert.match(JSON.stringify(value.result),/fixture-http/);
      writeFileSync(eventsPath,JSON.stringify(recorded));writeFileSync(resultPath,JSON.stringify(value));
    } else assert.deepEqual(value,JSON.parse(readFileSync(resultPath,'utf8')));
    console.log(`${phase} passed`);
  } catch(error) {await broker.abort().catch(()=>{});throw error;}
  finally {clearTimeout(timeout);if(server)await new Promise(resolve=>server.close(resolve));}
}
