import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, existsSync, unlinkSync, rmSync, openSync, closeSync } from 'node:fs';
import { createServer } from 'node:net';
import { createServer as createHttpServer } from 'node:http';
import { once } from 'node:events';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { NativeToolBroker } from '../assets/scripts/native-tool-broker.mjs';
import { replayEffects } from '../assets/scripts/native-snapshot-boundary.mjs';

const actual = mkdtempSync(join(tmpdir(), 'asb-qwen-native-'));
const fd = process.platform === 'linux' ? openSync(actual, 'r') : null;
const root = fd === null ? actual : `/proc/${process.pid}/fd/${fd}`;
try {
  const bundle=join(root,'scheduler.mjs'), entry=join(root,'client.mjs'), effect=join(root,'effect'), markerPath=join(root,'marker.sock'), socket=join(root,'broker.sock');
  writeFileSync(join(root, 'chunk-TR3TAG6E.js'), 'const fetchLike=baseFetch;');
  writeFileSync(bundle, `import {writeFileSync} from 'node:fs';
export function plain(scheduledCall,invocation,execSignal,liveOutputCallback,shellExecutionConfig){return invocation.execute(execSignal,liveOutputCallback,shellExecutionConfig)}
export function shell(scheduledCall,invocation,execSignal,liveOutputCallback,shellExecutionConfig,setPidCallback,setPromoteAbortControllerCallback,canPromoteForegroundShell){return invocation.execute(execSignal,liveOutputCallback,shellExecutionConfig,setPidCallback,setPromoteAbortControllerCallback,canPromoteForegroundShell)}
function scope(scheduledCall,signal,toolSpan){return ()=>this._executeToolCallBody(scheduledCall,signal,toolSpan)}
export const invocation = {execute: async()=>{writeFileSync(process.env.FIXTURE_EFFECT,'live');return {value:'done'}}};`);
  writeFileSync(entry, `import {writeFileSync} from 'node:fs'; import {plain,shell,invocation} from './scheduler.mjs';
const a=await plain({request:{name:'read_file',args:{path:'fixture'}}},invocation,1,2,3);
const b=await shell({request:{name:'run_shell_command',args:{command:'fixture'}}},invocation,1,2,3,4,5,6);
const delegate={execute:async()=>{writeFileSync(process.env.FIXTURE_DELEGATION,'ran');return plain({request:{name:'read_file',args:{path:'child'}}},invocation,1,2,3)}};
const c=await plain({request:{name:'Agent',args:{task:'fixture'}}},delegate,1,2,3);
if(a.value!=='done'||b.value!=='done'||c.value!=='done')process.exit(3);`);
  const markers=[];
  const controls=[];
  const provider=createHttpServer(async (request,response)=>{let body='';for await(const chunk of request)body+=chunk;assert.equal(request.url,'/v1/__asb_native_tool_consumed');controls.push(JSON.parse(body));response.writeHead(204);response.end();});
  await new Promise(resolve=>provider.listen(0,'127.0.0.1',resolve));
  let sequence=1;
  const marker=createServer(client=>client.once('data',data=>{const m=data.toString().trim();markers.push(m);if(m.startsWith('tool-end:')) sequence++;client.end(`${sequence}\n`);}));
  await new Promise(resolve=>marker.listen(markerPath,resolve));
  const bootstrap=fileURLToPath(new URL('../assets/scripts/qwen-native-bootstrap.mjs',import.meta.url));
  async function run(mode,events=[]) {
    const broker=new NativeToolBroker(mode,events,replayEffects(markerPath,'qwen-code')); await broker.listen(socket);
    const child=spawn(process.execPath,[entry],{cwd:root,env:{PATH:process.env.PATH||'',OPENAI_BASE_URL:`http://127.0.0.1:${provider.address().port}/v1`,NODE_OPTIONS:`--import=${JSON.stringify(bootstrap)}`,ASB_NATIVE_BUNDLE:bundle,ASB_NATIVE_MODE:mode,ASB_NATIVE_SOCKET:socket,ASB_NATIVE_MARKER:markerPath,FIXTURE_EFFECT:effect,FIXTURE_DELEGATION:join(root,'delegation')},stdio:['ignore','ignore','pipe']});
    let stderr=''; child.stderr.on('data',chunk=>stderr+=chunk);
    const [code]=await once(child,'exit'); assert.equal(code,0,stderr);
    return broker.finish();
  }
  const events=await run('record'); assert.equal(events.length,3); assert.ok(existsSync(effect)); unlinkSync(effect); unlinkSync(join(root,'delegation'));
  await run('replay',events); assert.equal(existsSync(effect),false,'replay must not execute native tools');
  assert.ok(existsSync(join(root,'delegation')),'delegated agent must run during replay');
  assert.deepEqual(controls, [0,1,2].map(ordinal=>({agent:'qwen-code',ordinal})));
  assert.equal(markers.filter(m=>m.startsWith('tool-begin:qwen-code-')).length,3);
  assert.equal(markers.filter(m=>m.startsWith('tool-end:qwen-code-')).length,6);
  await new Promise(resolve=>marker.close(resolve));
  await new Promise(resolve=>provider.close(resolve));
} finally { if(fd!==null)closeSync(fd); rmSync(actual,{recursive:true,force:true}); }
