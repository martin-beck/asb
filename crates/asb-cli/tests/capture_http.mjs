// Real embedded sidecar qualification against bounded loopback HTTP fixtures.
import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import { spawn, execFileSync } from "node:child_process";
import http from "node:http";
import net from "node:net";
import { setTimeout as delay } from "node:timers/promises";
import { normalizeWorkspacePaths } from "../assets/scripts/tape-normalization.mjs";

// Exercise an actual offline replay twice: first without a normalizer, then
// with either an unselected existing profile or a generated selected profile.
for (const variant of ['unselected-existing', 'generated-selected']) test(`replay requires ${variant} normalizer to complete`, {timeout:20000}, async t => {
  const parent=join(process.cwd(),'target/capture-http-fixtures');fs.mkdirSync(parent,{recursive:true});
  const root=fs.mkdtempSync(join(parent,'normalizer-required-'));
  const tape=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
  const body=(result,step)=>({model:'fixture',input:[
    {type:'function_call',call_id:'c1',name:'bash',arguments:JSON.stringify({command:'cat widget-report'})},
    {type:'function_call_output',call_id:'c1',output:result},
    ...(step===2?[{role:'user',content:'continue'}]:[]),
  ]});
  const expected='widget-drift=rose\nresult=ok', actual='widget-drift=lilac\nresult=ok';
  const line=step=>JSON.stringify({asb_request_policy:3,request:{method:'POST',url:'/v1/responses',body:JSON.stringify(body(expected,step))},response:{status:200,headers:{'content-type':'application/json'},body:JSON.stringify({step})}});
  const original=`${line(1)}\n${line(2)}\n`;
  async function launch(name, options={}) {
    const dir=join(root,name);fs.mkdirSync(dir);const cassette=join(dir,'cassette.jsonl'),ready=join(dir,'ready.json'),journal=join(dir,'journal.jsonl'),evidence=join(dir,'mismatch.json');
    fs.writeFileSync(cassette,original);
    const env={PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:'1',ASB_TAPE_READY_FILE:ready,ASB_TAPE_MISMATCH_EVIDENCE:evidence};
    if (options.catalog) {
      const catalog=join(dir,'catalog.json');fs.writeFileSync(catalog,JSON.stringify(options.catalog));
      env.ASB_TAPE_DISCOVERY_CATALOG=catalog;env.ASB_TAPE_IGNORED_MISMATCH_JOURNAL=journal;
    }
    if (options.selected) {
      const policy=join(dir,'policy.json');fs.writeFileSync(policy,JSON.stringify({sha256:'a'.repeat(64),workload:{id:'fixture',task_id:'one'},profiles:[{id:'selected-fix',source:options.selected}]}));
      env.ASB_TAPE_CUSTOM_NORMALIZERS=policy;
    }
    const child=spawn(process.execPath,[tape,'replay','--port','0','--cassette',cassette],{env,stdio:['pipe','ignore','pipe']});
    let stderr='';child.stderr.on('data',data=>{stderr+=data;});
    const exited=new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',(code,signal)=>resolve({code,signal}));});
    t.after(async()=>{if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');await exited;});
    const until=performance.now()+4000;while(!fs.existsSync(ready)){assert.equal(child.exitCode,null,stderr);assert.ok(performance.now()<until,stderr);await delay(10);}
    return {child,exited,dir,cassette,journal,evidence,port:JSON.parse(fs.readFileSync(ready)).port};
  }
  t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
  async function stop(session) {session.child.stdin.end();assert.deepEqual(await session.exited,{code:0,signal:null});}
  const baseline=await launch('baseline');
  assert.equal((await request(baseline.port,'/v1/responses',JSON.stringify(body(actual,1)))).status,409);
  await stop(baseline);
  const baselineProgress=JSON.parse(fs.readFileSync(`${baseline.cassette}.progress.json`));
  assert.equal(baselineProgress.consumed,0);assert.equal(baselineProgress.rejected,1);

  const source="export function normalizeToolResult({text,tool}) { return tool?.arguments?.command === 'cat widget-report' ? text.replace(/widget-drift=(?:rose|lilac)/,'widget-drift=<stable>') : text; }";
  const exploration=await launch('exploration',{catalog:variant==='unselected-existing'?[{id:'widget-drift',source}]:[]});
  for (const step of [1,2]) {
    const response=await request(exploration.port,'/v1/responses',JSON.stringify(body(actual,step)));
    assert.equal(response.status,200);assert.equal(JSON.parse(response.body).step,step);
  }
  await stop(exploration);
  const observed=JSON.parse(fs.readFileSync(`${exploration.cassette}.progress.json`));
  assert.equal(observed.consumed,2);assert.equal(observed.rejected,0);assert.equal(observed.ignored_mismatches,1);
  const event=JSON.parse(fs.readFileSync(exploration.journal,'utf8').trim());
  assert.equal(event.suggestion,variant==='unselected-existing'?'existing':'temporary');
  if (variant==='unselected-existing') assert.equal(event.normalizer,'widget-drift');
  if (variant==='generated-selected') {
    const generated=fs.readFileSync(event.file,'utf8');
    const selected=await launch('selected',{selected:generated});
    for (const step of [1,2]) {
      const response=await request(selected.port,'/v1/responses',JSON.stringify(body(actual,step)));
      assert.equal(response.status,200);assert.equal(JSON.parse(response.body).step,step);
    }
    await stop(selected);
    const verified=JSON.parse(fs.readFileSync(`${selected.cassette}.progress.json`));
    assert.equal(verified.consumed,2);assert.equal(verified.rejected,0);assert.equal(verified.ignored_mismatches,0);
    assert.equal(fs.existsSync(selected.evidence),false);
    assert.equal(fs.readFileSync(selected.cassette,'utf8'),original,'selected replay preserves raw cassette');
  }
});

for (const mode of ['existing', 'temporary']) test(`replay discovers ${mode} normalizer and stops at a non-tool divergence`, {timeout: 20000}, async (t) => {
  const parent = join(process.cwd(), 'target/capture-http-fixtures'); fs.mkdirSync(parent, {recursive:true});
  const root = fs.mkdtempSync(join(parent, 'discovery-'));
  const cassette = join(root, 'cassette.jsonl'), ready = join(root, 'ready.json');
  const journal = join(root, 'journal.jsonl'), evidence = join(root, 'mismatch.json'), catalog = join(root, 'catalog.json');
  const store = join(root, '.asb', 'replay-normalizer-exploration');
  fs.mkdirSync(join(root, '.asb'));
  const source = fs.readFileSync(fileURLToPath(new URL('../assets/examples/normalizers/python-unittest-elapsed.mjs', import.meta.url)), 'utf8');
  fs.writeFileSync(catalog, JSON.stringify(mode === 'existing' ? [{id:'python-unittest-elapsed',source}] : []));
  const recorded = mode === 'existing' ? 'Ran 1 test in 0.01s\n\nOK\n' : 'counter 101';
  const actual = mode === 'existing' ? 'Ran 1 test in 0.02s\n\nOK\n' : 'counter 202';
  const body = (text, suffix = '') => ({model:'fixture', input:[
    {type:'function_call',call_id:'call1',name:'bash',arguments:JSON.stringify({command:'python -m unittest tests'})},
    {type:'function_call_output',call_id:'call1',output:text},
    ...(suffix ? [{role:'user',content:suffix}] : []),
  ]});
  const exchange = (value, id) => JSON.stringify({asb_request_policy:3,request:{method:'POST',url:'/v1/responses',body:JSON.stringify(value)},response:{status:200,headers:{},body:JSON.stringify({id})}});
  fs.writeFileSync(cassette, `${exchange(body(recorded),1)}\n${exchange(body(recorded,'next'),2)}\n${exchange(body(recorded,'expected'),3)}\n`);
  const script=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
  const child=spawn(process.execPath,[script,'replay','--port','0','--cassette',cassette],{env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:'1',ASB_TAPE_READY_FILE:ready,ASB_TAPE_DISCOVERY_CATALOG:catalog,ASB_TAPE_DISCOVERY_STORE:store,ASB_TAPE_IGNORED_MISMATCH_JOURNAL:journal,ASB_TAPE_MISMATCH_EVIDENCE:evidence},stdio:['pipe','ignore','pipe']});
  let stderr='';child.stderr.on('data', data => {stderr += data;});
  const exited=new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',(code,signal)=>resolve({code,signal}));});
  t.after(async()=>{if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');await exited;fs.rmSync(root,{recursive:true,force:true});});
  const deadline=performance.now()+4000;while(!fs.existsSync(ready)){assert.equal(child.exitCode,null,stderr);assert.ok(performance.now()<deadline,stderr);await delay(10);}
  const port=JSON.parse(fs.readFileSync(ready)).port;
  assert.equal((await request(port,'/v1/responses',JSON.stringify(body(actual)))).status,200,stderr);
  assert.equal(fs.existsSync(store),false, 'a single matched response must not promote a candidate');
  const retried=await request(port,'/v1/responses',JSON.stringify(body(actual)));
  assert.equal(retried.status,200,stderr);assert.equal(JSON.parse(retried.body).id,1);
  assert.equal(fs.existsSync(store),false, 'a retry must not count as a later replay step');
  assert.equal((await request(port,'/v1/responses',JSON.stringify(body(actual,'next')))).status,200,stderr);
  if (mode === 'temporary') {
    const until=performance.now()+2000;
    while(!fs.existsSync(store) || !fs.readdirSync(store).some(name=>name.endsWith('.json'))) {assert.ok(performance.now()<until);await delay(10);}
    assert.equal(fs.readdirSync(store).filter(name=>name.endsWith('.json')).length,1);
  }
  assert.equal((await request(port,'/v1/responses',JSON.stringify(body(actual,'different')))).status,409,stderr);
  const events=fs.readFileSync(journal,'utf8').trim().split('\n').map(JSON.parse);
  assert.equal(events.length,1);
  assert.equal(events[0].suggestion,mode);
  assert.ok(fs.readFileSync(events[0].file,'utf8').includes('normalizeToolResult'));
  assert.equal(fs.statSync(events[0].file).mode & 0o077,0);
  const progress=JSON.parse(fs.readFileSync(`${cassette}.progress.json`));
  assert.equal(progress.consumed,2); assert.equal(progress.rejected,1); assert.equal(progress.ignored_mismatches,1);
  child.stdin.end(); assert.deepEqual(await exited,{code:0,signal:null},stderr);
  if (mode === 'temporary') {
    const secondRoot=join(root,'second');fs.mkdirSync(secondRoot);
    const secondReady=join(secondRoot,'ready.json'), secondJournal=join(secondRoot,'journal.jsonl'), secondEvidence=join(secondRoot,'mismatch.json');
    const next=spawn(process.execPath,[script,'replay','--port','0','--cassette',cassette],{env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:'1',ASB_TAPE_READY_FILE:secondReady,ASB_TAPE_DISCOVERY_CATALOG:catalog,ASB_TAPE_DISCOVERY_STORE:store,ASB_TAPE_IGNORED_MISMATCH_JOURNAL:secondJournal,ASB_TAPE_MISMATCH_EVIDENCE:secondEvidence},stdio:['pipe','ignore','pipe']});
    let err='';next.stderr.on('data',data=>{err+=data;});
    const nextExit=new Promise((resolve,reject)=>{next.once('error',reject);next.once('exit',(code,signal)=>resolve({code,signal}));});
    t.after(async()=>{if(next.exitCode===null&&next.signalCode===null)next.kill('SIGKILL');await nextExit;});
    const until=performance.now()+4000;while(!fs.existsSync(secondReady)){assert.equal(next.exitCode,null,err);assert.ok(performance.now()<until,err);await delay(10);}
    const nextPort=JSON.parse(fs.readFileSync(secondReady)).port;
    assert.equal((await request(nextPort,'/v1/responses',JSON.stringify(body(actual)))).status,200,err);
    const first=JSON.parse(fs.readFileSync(secondJournal,'utf8').trim());
    assert.equal(first.suggestion,'existing'); assert.match(first.normalizer,/^explore-/);
    next.stdin.end();assert.deepEqual(await nextExit,{code:0,signal:null},err);
  }
});

test('successive candidates are promoted and reused by a later replay', {timeout:20000}, async (t) => {
  const parent=join(process.cwd(),'target/capture-http-fixtures');fs.mkdirSync(parent,{recursive:true});
  const root=fs.mkdtempSync(join(parent,'iterative-discovery-'));fs.mkdirSync(join(root,'.asb'));
  const store=join(root,'.asb','replay-normalizer-exploration'), cassette=join(root,'cassette.jsonl'), catalog=join(root,'catalog.json');
  fs.writeFileSync(catalog,'[]');
  const call=(id,command,output)=>[{type:'function_call',call_id:id,name:'bash',arguments:JSON.stringify({command})},{type:'function_call_output',call_id:id,output}];
  const body=(first,second,step)=>({model:'fixture',input:[...call('a','cat one',first),...(step>=2?call('b','cat two',second):[]),...(step>=3?[{role:'user',content:'continue'}]:[])]});
  const line=(step)=>JSON.stringify({asb_request_policy:3,request:{method:'POST',url:'/v1/responses',body:JSON.stringify(body('one 1','two 1',step))},response:{status:200,headers:{},body:'{}'}});
  fs.writeFileSync(cassette,[1,2,3].map(line).join('\n')+'\n');
  const script=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
  async function run(number) {
    const dir=join(root,`attempt-${number}`);fs.mkdirSync(dir);
    const ready=join(dir,'ready.json'), journal=join(dir,'journal.jsonl'), evidence=join(dir,'mismatch.json');
    const child=spawn(process.execPath,[script,'replay','--port','0','--cassette',cassette],{env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:'1',ASB_TAPE_READY_FILE:ready,ASB_TAPE_DISCOVERY_CATALOG:catalog,ASB_TAPE_DISCOVERY_STORE:store,ASB_TAPE_IGNORED_MISMATCH_JOURNAL:journal,ASB_TAPE_MISMATCH_EVIDENCE:evidence},stdio:['pipe','ignore','pipe']});
    let stderr='';child.stderr.on('data',data=>{stderr+=data;});
    const exited=new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',(code,signal)=>resolve({code,signal}));});
    t.after(async()=>{if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');await exited;});
    const until=performance.now()+4000;while(!fs.existsSync(ready)){assert.equal(child.exitCode,null,stderr);assert.ok(performance.now()<until,stderr);await delay(10);}
    const port=JSON.parse(fs.readFileSync(ready)).port;
    for (const step of [1,2,3]) assert.equal((await request(port,'/v1/responses',JSON.stringify(body('one 2','two 2',step)))).status,200,stderr);
    const events=fs.readFileSync(journal,'utf8').trim().split('\n').map(JSON.parse);
    const wait=performance.now()+2000;while(!fs.existsSync(store)||fs.readdirSync(store).filter(n=>n.endsWith('.json')).length<2){assert.ok(performance.now()<wait,stderr);await delay(10);}
    child.stdin.end();assert.deepEqual(await exited,{code:0,signal:null},stderr);
    return events;
  }
  t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
  const first=await run(1);assert.deepEqual(first.map(e=>e.suggestion),['temporary','temporary']);
  const second=await run(2);assert.deepEqual(second.map(e=>e.suggestion),['existing','existing']);
  assert.ok(second.every(e=>e.normalizer.startsWith('explore-')));
});

test('a replay already running discovers a candidate promoted by a peer', {timeout:20000}, async (t) => {
  const parent=join(process.cwd(),'target/capture-http-fixtures');fs.mkdirSync(parent,{recursive:true});
  const root=fs.mkdtempSync(join(parent,'concurrent-discovery-'));fs.mkdirSync(join(root,'.asb'));
  const store=join(root,'.asb','replay-normalizer-exploration'), cassette=join(root,'cassette.jsonl'), catalog=join(root,'catalog.json');fs.writeFileSync(catalog,'[]');
  const body=(output,more=false)=>({model:'fixture',input:[{type:'function_call',call_id:'a',name:'bash',arguments:'{"command":"cat one"}'},{type:'function_call_output',call_id:'a',output},...(more?[{role:'user',content:'next'}]:[])]});
  const row=(more)=>JSON.stringify({asb_request_policy:3,request:{method:'POST',url:'/v1/responses',body:JSON.stringify(body('before',more))},response:{status:200,headers:{},body:'{}'}});
  fs.writeFileSync(cassette,`${row(false)}\n${row(true)}\n`);
  const script=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
  async function launch(name) {
    const dir=join(root,name);fs.mkdirSync(dir);const ready=join(dir,'ready.json'),journal=join(dir,'journal.jsonl');
    const child=spawn(process.execPath,[script,'replay','--port','0','--cassette',cassette],{env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:'1',ASB_TAPE_READY_FILE:ready,ASB_TAPE_DISCOVERY_CATALOG:catalog,ASB_TAPE_DISCOVERY_STORE:store,ASB_TAPE_IGNORED_MISMATCH_JOURNAL:journal,ASB_TAPE_MISMATCH_EVIDENCE:join(dir,'mismatch.json')},stdio:['pipe','ignore','pipe']});
    let stderr='';child.stderr.on('data',data=>{stderr+=data;});
    const exited=new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',(code,signal)=>resolve({code,signal}));});
    t.after(async()=>{if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');await exited;});
    const until=performance.now()+4000;while(!fs.existsSync(ready)){assert.equal(child.exitCode,null,stderr);assert.ok(performance.now()<until,stderr);await delay(10);}
    return {child,exited,journal,port:JSON.parse(fs.readFileSync(ready)).port};
  }
  t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
  const first=await launch('first'), peer=await launch('peer');
  assert.equal((await request(first.port,'/v1/responses',JSON.stringify(body('after')))).status,200);
  assert.equal((await request(first.port,'/v1/responses',JSON.stringify(body('after',true)))).status,200);
  const until=performance.now()+2000;while(!fs.existsSync(store)||!fs.readdirSync(store).some(n=>n.endsWith('.json'))){assert.ok(performance.now()<until);await delay(10);}
  assert.equal((await request(peer.port,'/v1/responses',JSON.stringify(body('after')))).status,200);
  const event=JSON.parse(fs.readFileSync(peer.journal,'utf8').trim());
  assert.equal(event.suggestion,'existing');assert.match(event.normalizer,/^explore-/);
  first.child.stdin.end();peer.child.stdin.end();assert.deepEqual(await first.exited,{code:0,signal:null});assert.deepEqual(await peer.exited,{code:0,signal:null});
});

test('a stateful candidate that fails whole-request verification falls back to a temporary rule', {timeout:15000}, async (t) => {
  const parent=join(process.cwd(),'target/capture-http-fixtures');fs.mkdirSync(parent,{recursive:true});
  const root=fs.mkdtempSync(join(parent,'stateful-discovery-'));
  const cassette=join(root,'cassette.jsonl'),ready=join(root,'ready.json'),journal=join(root,'journal.jsonl'),catalog=join(root,'catalog.json');
  const source="let n=0; export function normalizeToolResult({text}) { n+=1; return n<=2 ? 'same' : text; }";
  fs.writeFileSync(catalog,JSON.stringify([{id:'stateful',source}]));
  const body=value=>({model:'fixture',input:[{type:'function_call',call_id:'a',name:'bash',arguments:'{"command":"cat x"}'},{type:'function_call_output',call_id:'a',output:'same output'},{type:'function_call_output',call_id:'a',output:value}]});
  fs.writeFileSync(cassette,JSON.stringify({asb_request_policy:3,request:{method:'POST',url:'/v1/responses',body:JSON.stringify(body('recorded'))},response:{status:200,headers:{},body:'{}'}})+'\n');
  const script=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
  const child=spawn(process.execPath,[script,'replay','--port','0','--cassette',cassette],{env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:'1',ASB_TAPE_READY_FILE:ready,ASB_TAPE_DISCOVERY_CATALOG:catalog,ASB_TAPE_IGNORED_MISMATCH_JOURNAL:journal,ASB_TAPE_MISMATCH_EVIDENCE:join(root,'mismatch.json')},stdio:['pipe','ignore','pipe']});
  let stderr='';child.stderr.on('data',data=>{stderr+=data;});
  const exited=new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',(code,signal)=>resolve({code,signal}));});
  t.after(async()=>{if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');await exited;fs.rmSync(root,{recursive:true,force:true});});
  const until=performance.now()+4000;while(!fs.existsSync(ready)){assert.equal(child.exitCode,null,stderr);assert.ok(performance.now()<until,stderr);await delay(10);}
  assert.equal((await request(JSON.parse(fs.readFileSync(ready)).port,'/v1/responses',JSON.stringify(body('actual')))).status,200,stderr);
  const event=JSON.parse(fs.readFileSync(journal,'utf8').trim());assert.equal(event.suggestion,'temporary');
  const progress=JSON.parse(fs.readFileSync(`${cassette}.progress.json`));assert.ok(progress.normalizer_exploration.catalog_failures>=1);
  child.stdin.end();assert.deepEqual(await exited,{code:0,signal:null});
});

test('discovered normalization preserves ordering for concurrent later requests', {timeout:15000}, async (t) => {
  const parent=join(process.cwd(),'target/capture-http-fixtures');fs.mkdirSync(parent,{recursive:true});
  const root=fs.mkdtempSync(join(parent,'discovery-order-'));
  const cassette=join(root,'cassette.jsonl'),ready=join(root,'ready.json'),journal=join(root,'journal.jsonl'),catalog=join(root,'catalog.json');fs.writeFileSync(catalog,'[]');
  const body=(output,turn)=>({model:'fixture',input:[{type:'function_call',call_id:'a',name:'bash',arguments:'{"command":"cat x"}'},{type:'function_call_output',call_id:'a',output},...(turn?[{role:'user',content:turn}]:[])]});
  const row=(turn,id)=>JSON.stringify({asb_request_policy:3,request:{method:'POST',url:'/v1/responses',body:JSON.stringify(body('recorded',turn))},response:{status:200,headers:{},body:JSON.stringify({id})}});
  fs.writeFileSync(cassette,[row('',1),row('second',2),row('third',3)].join('\n')+'\n');
  const script=fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
  const child=spawn(process.execPath,[script,'replay','--port','0','--cassette',cassette],{env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:'1',ASB_TAPE_READY_FILE:ready,ASB_TAPE_DISCOVERY_CATALOG:catalog,ASB_TAPE_IGNORED_MISMATCH_JOURNAL:journal,ASB_TAPE_MISMATCH_EVIDENCE:join(root,'mismatch.json')},stdio:['pipe','ignore','pipe']});
  let stderr='';child.stderr.on('data',data=>{stderr+=data;});
  const exited=new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',(code,signal)=>resolve({code,signal}));});
  t.after(async()=>{if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');await exited;fs.rmSync(root,{recursive:true,force:true});});
  const until=performance.now()+4000;while(!fs.existsSync(ready)){assert.equal(child.exitCode,null,stderr);assert.ok(performance.now()<until,stderr);await delay(10);}
  const port=JSON.parse(fs.readFileSync(ready)).port;
  assert.equal(JSON.parse((await request(port,'/v1/responses',JSON.stringify(body('actual','')))).body).id,1);
  const third=request(port,'/v1/responses',JSON.stringify(body('actual','third')));await delay(50);
  const second=request(port,'/v1/responses',JSON.stringify(body('actual','second')));
  assert.equal(JSON.parse((await second).body).id,2);assert.equal(JSON.parse((await third).body).id,3);
  assert.equal(fs.readFileSync(journal,'utf8').trim().split('\n').length,1);
  child.stdin.end();assert.deepEqual(await exited,{code:0,signal:null},stderr);
});

test("ignored mismatch journal write failure makes replay shutdown fail", { timeout: 10000 }, async (t) => {
  const parent = join(process.cwd(), "target/capture-http-fixtures");
  fs.mkdirSync(parent, {recursive:true});
  const root = fs.mkdtempSync(join(parent, "mismatch-journal-"));
  const cassette = join(root, "cassette.jsonl");
  const ready = join(root, "ready.json");
  fs.writeFileSync(cassette, `${JSON.stringify({request:{method:"POST",url:"/v1/responses",body:JSON.stringify({input:"original"})},response:{status:200,headers:{},body:"{}"}})}\n`);
  const script = fileURLToPath(new URL("../assets/scripts/llmtape.mjs", import.meta.url));
  const child = spawn(process.execPath, [script,"replay","--port","0","--cassette",cassette], {env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:"1",ASB_TAPE_READY_FILE:ready,ASB_TAPE_IGNORE_PROVIDER_MISMATCH:"1",ASB_TAPE_IGNORED_MISMATCH_JOURNAL:join(root,"missing","journal.jsonl")},stdio:["pipe","ignore","pipe"]});
  const exited = new Promise((resolve, reject) => {child.once("error",reject);child.once("exit",(code, signal) => resolve({code, signal}));});
  t.after(async () => { if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL"); await exited; fs.rmSync(root,{recursive:true,force:true}); });
  const deadline = performance.now() + 3000;
  while (!fs.existsSync(ready)) { assert.ok(performance.now()<deadline, "replay readiness timeout"); await delay(10); }
  const port = JSON.parse(fs.readFileSync(ready)).port;
  assert.equal((await request(port,"/v1/responses",JSON.stringify({input:"changed"}))).status,502);
  child.stdin.end();
  assert.deepEqual(await exited,{code:2,signal:null});
});

test("concurrent ignored provider mismatches consume different recorded exchanges", { timeout: 10000 }, async (t) => {
  const parent = join(process.cwd(), "target/capture-http-fixtures"); fs.mkdirSync(parent, {recursive:true});
  const root = fs.mkdtempSync(join(parent, "mismatch-concurrent-"));
  const cassette = join(root,"cassette.jsonl"), ready = join(root,"ready.json"), journal = join(root,"journal.jsonl"), wire = join(root,"wire.mjs");
  fs.writeFileSync(wire,'export async function observeReplayExchange() { await new Promise(resolve => setTimeout(resolve,100)); }');
  const exchange = (id) => JSON.stringify({request:{method:"POST",url:"/v1/responses",body:JSON.stringify({input:`original-${id}`})},response:{status:200,headers:{},body:JSON.stringify({id})}});
  fs.writeFileSync(cassette,`${exchange(1)}\n${exchange(2)}\n`);
  const script=fileURLToPath(new URL("../assets/scripts/llmtape.mjs",import.meta.url));
  const child=spawn(process.execPath,[script,"replay","--port","0","--cassette",cassette],{env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:"1",ASB_TAPE_READY_FILE:ready,ASB_TAPE_IGNORE_PROVIDER_MISMATCH:"1",ASB_TAPE_IGNORED_MISMATCH_JOURNAL:journal,ASB_TAPE_AGENT_WIRE_EXTENSION:wire},stdio:["pipe","ignore","pipe"]});
  const exited=new Promise((resolve,reject)=>{child.once("error",reject);child.once("exit",(code,signal)=>resolve({code,signal}));});
  t.after(async()=>{if(child.exitCode===null&&child.signalCode===null) child.kill("SIGKILL");await exited;fs.rmSync(root,{recursive:true,force:true});});
  const deadline=performance.now()+3000;while(!fs.existsSync(ready)){assert.ok(performance.now()<deadline);await delay(10);}
  const port=JSON.parse(fs.readFileSync(ready)).port;
  const replies=await Promise.all([request(port,"/v1/responses",JSON.stringify({input:"changed-a"})),request(port,"/v1/responses",JSON.stringify({input:"changed-b"}))]);
  assert.deepEqual(replies.map(reply=>JSON.parse(reply.body).id).sort(),[1,2]);
  const events=fs.readFileSync(journal,"utf8").trim().split("\n").map(line=>JSON.parse(line));
  assert.deepEqual(events.map(event=>event.sequence),[1,2]);
  child.stdin.end();assert.deepEqual(await exited,{code:0,signal:null});
});

async function fixture(t, upstreamHandler, options = {}) {
  const parent = join(process.cwd(), "target/capture-http-fixtures");
  fs.mkdirSync(parent, { recursive: true });
  const root = fs.mkdtempSync(join(parent, "capture-"));
  const upstream = http.createServer(upstreamHandler);
  await new Promise((resolve) => upstream.listen(0, "127.0.0.1", resolve));
  const ready = join(root, "ready.json");
  const output = join(root, "trace.jsonl");
  const receipt = join(root, "receipt.json");
  const policy = join(root, "policy.json");
  const normalizationEvidence = join(root, "normalization.json");
  if (options.policy) fs.writeFileSync(policy, JSON.stringify(options.policy));
  const agentNormalizer = join(root, "agent-normalizer.mjs");
  if (options.agentNormalizer) fs.writeFileSync(agentNormalizer, options.agentNormalizer);
  const curlMarker = join(root, "curl.pid");
  let path = process.env.PATH;
  if (options.curl) {
    const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;
    const bin = join(root, "bin");
    fs.mkdirSync(bin);
    const script = options.resistant
      ? `#!/bin/sh\nexec ${quote(process.execPath)} -e ${quote('process.on("SIGTERM",()=>{}); require("fs").writeFileSync(process.argv[1],String(process.pid)); setInterval(()=>{},1000);')} ${quote(curlMarker)}\n`
      : `#!/bin/sh\nprintf '%s' "$$" > ${quote(curlMarker)}\nexec ${quote(execFileSync("which", ["curl"], { encoding: "utf8" }).trim())} "$@"\n`;
    fs.writeFileSync(join(bin, "curl"), script, { mode: 0o700 });
    path = `${bin}:${path}`;
  }
  const tapeScript = fileURLToPath(new URL("../assets/scripts/llmtape.mjs", import.meta.url));
  const arguments_ = [
    tapeScript,
    "record", "--port", "0", "--upstream", `http://127.0.0.1:${upstream.address().port}`, "--out", output,
  ];
  const child = spawn(process.execPath, arguments_, {
    env: { PATH: path, HOME: root, ASB_TAPE_OWNER_PIPE: "1", ASB_TAPE_READY_FILE: ready,
      ...(options.curl ? { http_proxy: "http://127.0.0.1:1", no_proxy: "127.0.0.1" } : {}),
      ASB_TAPE_CAPTURE_RECEIPT: receipt, ASB_TAPE_CAPTURE_NONCE: "fixture-private-invocation",
      ...(options.policy ? { ASB_TAPE_CUSTOM_NORMALIZERS: policy, ASB_TAPE_NORMALIZATION_EVIDENCE: normalizationEvidence } : {}),
      ...(options.agentNormalizer ? { ASB_TAPE_AGENT_NORMALIZER: agentNormalizer } : {}),
      ...(options.workspace ? { ASB_TAPE_WORKSPACE: options.workspace } : {}) },
    stdio: ["pipe", "ignore", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (data) => { stderr = (stderr + data).slice(-4096); });
  const exited = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve({ code, signal }));
  });
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    await exited;
    upstream.closeAllConnections();
    await new Promise((resolve) => upstream.close(resolve));
    fs.rmSync(root, { recursive: true });
  });
  const deadline = performance.now() + 2000;
  while (!fs.existsSync(ready)) {
    assert.equal(child.exitCode, null, stderr);
    assert.ok(performance.now() < deadline, `readiness timeout: ${stderr}`);
    await delay(10);
  }
  const port = JSON.parse(fs.readFileSync(ready)).port;
  async function stop() {
    child.stdin.end();
    const result = await Promise.race([
      exited,
      delay(2500, undefined, { ref: false }).then(() => { throw new Error(`sidecar shutdown timeout: ${stderr}`); }),
    ]);
    assert.deepEqual(result, { code: 0, signal: null }, stderr);
    const value = JSON.parse(fs.readFileSync(receipt));
    const bytes = fs.readFileSync(output);
    assert.equal(value.pid, child.pid);
    assert.equal(value.nonce, "fixture-private-invocation");
    assert.equal(value.bytes, bytes.length);
    assert.equal(value.sha256, createHash("sha256").update(bytes).digest("hex"));
    assert.equal(value.inflight, 0);
    assert.equal(value.accepted, value.completed + value.failed);
    return { value, bytes };
  }
  return { port, stop, child, curlMarker, output, normalizationEvidence };
}

test("recording stores original requests and responses without invoking normalizers", { timeout: 10000 }, async (t) => {
  let forwarded = "";
  const policy = { sha256: "a".repeat(64), workload: { id: "fixture", task_id: "one" }, profiles: [{ id: "never-call", source: "export function normalizeToolResult() { throw new Error('must not run during recording'); }" }] };
  const workspace = "/tmp/record/workspace-concurrency-1";
  const responseBody = JSON.stringify({ message: `${workspace}/file`, raw: "seed=1" });
  const { port, stop, normalizationEvidence } = await fixture(t, (req, res) => {
    req.setEncoding("utf8"); req.on("data", (chunk) => { forwarded += chunk; });
    req.on("end", () => { res.writeHead(200, { "content-type": "application/json" }); res.end(responseBody); });
  }, { policy, workspace });
  const body = JSON.stringify({ model: "m", messages: [{ role: "user", content: "seed=1" }, { role: "tool", content: `seed=1 ${workspace}` }] });
  const response = await request(port, "/v1/chat/completions", body);
  assert.equal(response.status, 200);
  assert.equal(response.body, responseBody);
  const { value, bytes } = await stop();
  assert.equal(value.complete, true);
  assert.equal(forwarded, body);
  const saved = JSON.parse(bytes.toString());
  assert.equal(saved.asb_request_policy, 3);
  assert.equal(saved.recorded_workspace, workspace);
  assert.equal(saved.request.body, body);
  assert.equal(saved.response.body, responseBody);
  assert.equal(fs.existsSync(normalizationEvidence), false);
});

test("recording forwards and stores an unmodified Responses request", { timeout: 10000 }, async (t) => {
  let forwarded = "";
  const { port, stop } = await fixture(t, (req, res) => {
    req.setEncoding("utf8"); req.on("data", (chunk) => { forwarded += chunk; });
    req.on("end", () => res.end("{}"));
  });
  const body = JSON.stringify({model:"m", reasoning:{effort:"low", context:"original"}, input:[{type:"additional_tools", tools:[]}]});
  assert.equal((await request(port, "/v1/responses", body)).status, 200);
  const { bytes } = await stop();
  assert.equal(forwarded, body);
  assert.equal(JSON.parse(bytes.toString()).request.body, body);
});

test("profile matching leaves unrelated prompts, arguments and tool text significant", { timeout: 10000 }, async (t) => {
  const policy = { sha256: "a".repeat(64), workload: { id: "fixture", task_id: "one" }, profiles: [{
    id: "only-seed", source: "export function normalizeToolResult({text}) { return text.replace('seed=1', 'seed=<seed>'); }",
  }] };
  const { port, stop } = await fixture(t, (_req, res) => res.end("{}"), { policy });
  const text = "1 passed in 1.23s\n<Thing object at 0xabc>\n../foo/AGENTS.md\nChunk ID: abcdef";
  const body = JSON.stringify({ model: "m", messages: [
    { role: "user", content: text },
    { role: "assistant", tool_calls: [{ id: "a", function: { name: "bash", arguments: JSON.stringify({ command: text }) } }] },
    { role: "tool", tool_call_id: "a", content: `${text}\nseed=1` },
  ] });
  assert.equal((await request(port, "/v1/chat/completions", body)).status, 200);
  const { bytes } = await stop();
  const saved = JSON.parse(JSON.parse(bytes.toString()).request.body);
  assert.equal(saved.messages[0].content, text);
  assert.equal(JSON.parse(saved.messages[1].tool_calls[0].function.arguments).command, text);
  assert.equal(saved.messages[2].content, `${text}\nseed=1`);
});

test("agent envelope precedes workload profile while original call associations survive", { timeout: 10000 }, async (t) => {
  let forwarded = "";
  const policy = { sha256: "a".repeat(64), workload: { id: "fixture", task_id: "one" }, profiles: [{
    id: "specific", source: "export function normalizeToolResult({text,tool}) { return tool?.arguments?.command === 'first' ? text + ':profile' : text; }",
  }] };
  const agentNormalizer = `export function normalizeRequest({body}) {
    for (const item of body.input) {
      if (item.call_id) item.call_id = 'collapsed';
      if (item.type === 'function_call_output') item.output += ':agent';
    }
    return body;
  }`;
  const { port, stop } = await fixture(t, (req, res) => {
    req.setEncoding("utf8"); req.on("data", (chunk) => { forwarded += chunk; }); req.on("end", () => res.end("{}"));
  }, { policy, agentNormalizer });
  const body = JSON.stringify({ input: [
    { type: "function_call", call_id: "a", name: "bash", arguments: '{"command":"first"}' },
    { type: "function_call_output", call_id: "a", output: "one" },
    { type: "function_call", call_id: "b", name: "bash", arguments: '{"command":"second"}' },
    { type: "function_call_output", call_id: "b", output: "two" },
  ] });
  assert.equal((await request(port, "/v1/responses", body)).status, 200);
  const { bytes } = await stop();
  assert.equal(forwarded, body);
  const saved = JSON.parse(JSON.parse(bytes.toString()).request.body);
  assert.equal(saved.input[1].output, "one");
  assert.equal(saved.input[3].output, "two");
});

test("custom policy applies once on replay and rejects changed prompts", { timeout: 10000 }, async (t) => {
  const policy = { sha256: "a".repeat(64), workload: { id: "fixture", task_id: "one" }, profiles: [{ id: "seed", source: "export function normalizeToolResult({text}) { return text.replace(/seed=\\d+/, 'seed=<seed>') + '!'; }" }] };
  const oldWorkspace = "/tmp/record/workspace-concurrency-1";
  const newWorkspace = "/tmp/replay/workspace-concurrency-2";
  const { port, stop, output } = await fixture(t, (_req, res) => res.end(JSON.stringify({ path: `${oldWorkspace}/src`, unrelated: "/tmp/elsewhere", literal: "<asb-workspace>" })), { policy, workspace: oldWorkspace });
  const recorded = JSON.stringify({ model: "m", messages: [{ role: "user", content: `solve in ${oldWorkspace}` }, { role: "tool", content: "seed=1" }] });
  assert.equal((await request(port, "/v1/chat/completions", recorded)).status, 200);
  await stop();
  const root = join(process.cwd(), "target/capture-http-fixtures");
  const replayRoot = fs.mkdtempSync(join(root, "replay-"));
  const cassette = join(replayRoot, "cassette.jsonl");
  const policyPath = join(replayRoot, "policy.json");
  const ready = join(replayRoot, "ready.json");
  fs.copyFileSync(output, cassette);
  fs.writeFileSync(policyPath, JSON.stringify(policy));
  const script = fileURLToPath(new URL("../assets/scripts/llmtape.mjs", import.meta.url));
  const child = spawn(process.execPath, [script, "replay", "--port", "0", "--cassette", cassette], {
    env: { PATH: process.env.PATH, ASB_TAPE_OWNER_PIPE: "1", ASB_TAPE_READY_FILE: ready, ASB_TAPE_CUSTOM_NORMALIZERS: policyPath, ASB_TAPE_WORKSPACE: newWorkspace },
    stdio: ["pipe", "ignore", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (data) => { stderr += data; });
  const exited = new Promise((resolve) => child.once("exit", (code, signal) => resolve({ code, signal })));
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    await exited;
    fs.rmSync(replayRoot, { recursive: true });
  });
  const deadline = performance.now() + 3000;
  while (!fs.existsSync(ready)) {
    assert.equal(child.exitCode, null, stderr);
    assert.ok(performance.now() < deadline, stderr);
    await delay(10);
  }
  const replayPort = JSON.parse(fs.readFileSync(ready)).port;
  const changed = JSON.stringify({ model: "m", messages: [{ role: "user", content: `changed in ${newWorkspace}` }, { role: "tool", content: "seed=2" }] });
  assert.equal((await request(replayPort, "/v1/chat/completions", changed)).status, 409);
  const replayed = JSON.stringify({ model: "m", messages: [{ role: "user", content: `solve in ${newWorkspace}` }, { role: "tool", content: "seed=2" }] });
  const replayResponse = await request(replayPort, "/v1/chat/completions", replayed);
  assert.equal(replayResponse.status, 200);
  assert.deepEqual(JSON.parse(replayResponse.body), { path: `${newWorkspace}/src`, unrelated: "/tmp/elsewhere", literal: "<asb-workspace>" });
  assert.equal(JSON.parse(fs.readFileSync(cassette, "utf8")).response.body, JSON.stringify({ path: `${oldWorkspace}/src`, unrelated: "/tmp/elsewhere", literal: "<asb-workspace>" }));
  child.stdin.end();
  assert.deepEqual(await exited, { code: 0, signal: null }, stderr);
  const progress = JSON.parse(fs.readFileSync(`${cassette}.progress.json`));
  assert.equal(progress.consumed, 1);
  assert.equal(progress.rejected, 1);

});

test("Claude parallel results replay by call identity after scoped text normalization", {timeout:10000}, async (t) => {
  const policy = {sha256:'b'.repeat(64), workload:{id:'fixture',task_id:'one'}, profiles:[{id:'tool', source:"export function normalizeToolResult({text,tool}) { return tool.name + ':' + text.replace(/seed=\\d+/, 'seed=<seed>'); }"}]};
  const {port, stop, output} = await fixture(t, (_req,res) => res.end('{}'), {policy});
  const calls = [{type:'tool_use',id:'call-a',name:'Read',input:{path:'a'}},{type:'tool_use',id:'call-b',name:'Bash',input:{command:'b'}}];
  const a = {type:'tool_result',tool_use_id:'call-a',content:'seed=1'};
  const b = {type:'tool_result',tool_use_id:'call-b',content:'output'};
  const body = content => JSON.stringify({model:'m',messages:[{role:'assistant',content:calls},{role:'user',content}]});
  assert.equal((await request(port,'/v1/messages',body([a,{...b,cache_control:{type:'ephemeral'}}]))).status,200);
  await stop();
  const root = fs.mkdtempSync(join(process.cwd(),'target/capture-http-fixtures/replay-parallel-'));
  const cassette = join(root,'cassette.jsonl'), policyPath = join(root,'policy.json'), ready = join(root,'ready.json');
  fs.copyFileSync(output,cassette); fs.writeFileSync(policyPath,JSON.stringify(policy));
  const script = fileURLToPath(new URL('../assets/scripts/llmtape.mjs',import.meta.url));
  const normalizer = fileURLToPath(new URL('../assets/scripts/agent-normalizers/claude-code.mjs',import.meta.url));
  const child = spawn(process.execPath,[script,'replay','--port','0','--cassette',cassette],{
    env:{PATH:process.env.PATH,ASB_TAPE_OWNER_PIPE:'1',ASB_TAPE_READY_FILE:ready,ASB_TAPE_CUSTOM_NORMALIZERS:policyPath,ASB_TAPE_AGENT_NORMALIZER:normalizer},stdio:['pipe','ignore','pipe']});
  let stderr=''; child.stderr.on('data',data=>{stderr+=data});
  const exited=new Promise(resolve=>child.once('exit',(code,signal)=>resolve({code,signal})));
  t.after(async()=>{if(child.exitCode===null&&child.signalCode===null) child.kill('SIGKILL'); await exited; fs.rmSync(root,{recursive:true});});
  const deadline=performance.now()+3000;
  while(!fs.existsSync(ready)){assert.equal(child.exitCode,null,stderr); assert.ok(performance.now()<deadline,stderr); await delay(10);}
  const replayPort=JSON.parse(fs.readFileSync(ready)).port;
  assert.equal((await request(replayPort,'/v1/messages',body([{...b,content:'changed'},{...a,cache_control:{type:'ephemeral'}}]))).status,409);
  assert.equal((await request(replayPort,'/v1/messages',body([b,{...a,content:'seed=2',cache_control:{type:'ephemeral'}}]))).status,200);
  child.stdin.end(); assert.deepEqual(await exited,{code:0,signal:null},stderr);
});

test("a failing replay normalizer rejects the raw cassette without changing it", { timeout: 10000 }, async (t) => {
  const { port, stop, output } = await fixture(t, (_req, res) => res.end("{}"));
  const body = JSON.stringify({ model: "m", messages: [{ role: "tool", content: "result" }] });
  assert.equal((await request(port, "/v1/chat/completions", body)).status, 200);
  await stop();
  const before = fs.readFileSync(output);
  const policyPath = `${output}.policy.json`;
  fs.writeFileSync(policyPath, JSON.stringify({ sha256: "a".repeat(64), workload: {id:"fixture", task_id:"one"}, profiles: [{ id:"broken", source:"export function normalizeToolResult() { throw new Error('private error'); }" }] }));
  const script = fileURLToPath(new URL("../assets/scripts/llmtape.mjs", import.meta.url));
  const child = spawn(process.execPath, [script, "replay", "--port", "0", "--cassette", output], {
    env: {PATH:process.env.PATH, ASB_TAPE_CUSTOM_NORMALIZERS:policyPath, ASB_TAPE_READY_FILE:`${output}.ready.json`}, stdio:["ignore", "ignore", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (data) => { stderr += data; });
  const result = await new Promise((resolve) => child.once("exit", (code) => resolve(code)));
  assert.notEqual(result, 0);
  assert.match(stderr, /normalizer broken/);
  assert.doesNotMatch(stderr, /private error/);
  assert.deepEqual(fs.readFileSync(output), before);
});

test("oversized upload returns 413 but cannot become a complete recording", { timeout: 10000 }, async (t) => {
  const { port, stop } = await fixture(t, () => assert.fail("oversized upload reached upstream"));
  const result = await request(port, "/v1/responses", "x".repeat(16 * 1024 * 1024 + 1));
  assert.equal(result.status, 413);
  const { value } = await stop();
  assert.equal(value.accepted, 1);
  assert.equal(value.failed, 1);
  assert.equal(value.recorded, 0);
  assert.equal(value.complete, false);
});

test("large non-path response normalization stays bounded and captures a full trace", { timeout: 10000 }, async (t) => {
  const { port, stop } = await fixture(t, (_req, res) => res.end("x".repeat(12 * 1024 * 1024)));
  const result = await request(port);
  assert.equal(result.body.length, 12 * 1024 * 1024);
  const { value } = await stop();
  assert.equal(value.accepted, 1);
  assert.equal(value.recorded, 1);
  assert.ok(value.bytes > 12 * 1024 * 1024);
  assert.equal(value.failed, 0);
  assert.equal(value.complete, true);
});

for (const resistant of [false, true]) {
  test(`curl backend quiesces owned ${resistant ? "TERM-resistant fixture" : "actual curl"} before receipt`, { timeout: 10000, skip: process.platform !== "linux" }, async (t) => {
    let announce;
    const upstreamSeen = new Promise((resolve) => { announce = resolve; });
    const { port, stop, child, curlMarker } = await fixture(t, () => announce(), { curl: true, resistant });
    const sentinel = spawn(process.execPath, ["-e", "setInterval(()=>{},1000)"], { stdio: "ignore" });
    const sentinelExit = new Promise((resolve) => sentinel.once("exit", resolve));
    t.after(async () => { sentinel.kill("SIGKILL"); await sentinelExit; });
    const req = http.request({ host: "127.0.0.1", port, path: "/v1/responses", method: "POST" });
    req.on("error", () => {});
    req.on("response", (res) => { res.on("error", () => {}); res.resume(); });
    req.end("{}");
    const deadline = performance.now() + 2000;
    while (!fs.existsSync(curlMarker)) {
      assert.ok(performance.now() < deadline, "owned curl fixture never started");
      await delay(10);
    }
    if (!resistant) await upstreamSeen;
    const owned = Number(fs.readFileSync(curlMarker, "utf8"));
    assert.ok(Number.isSafeInteger(owned) && owned > 1);
    assert.ok(fs.existsSync(`/proc/${owned}`), "actual owned child must be live before stop");
    const stopped = stop();
    await delay(10);
    child.kill("SIGTERM"); // Repeated shutdown during drain must not publish twice.
    const { value } = await stopped;
    assert.equal(value.accepted, 1);
    assert.equal(value.failed, 1);
    assert.equal(value.complete, false);
    assert.equal(fs.existsSync(`/proc/${owned}`), false, "owned outbound child must actually be reaped");
    assert.equal(sentinel.exitCode, null, "unrelated process must survive outbound cleanup");
  });
}

function request(port, path = "/v1/responses", body = "{}") {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: "127.0.0.1", port, path, method: body === null ? "GET" : "POST" }, (res) => {
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.once("error", reject);
      res.once("end", () => resolve({ status: res.statusCode, body: Buffer.concat(chunks).toString() }));
    });
    req.once("error", reject);
    req.end(body);
  });
}

test("legacy request normalization retains workspace key compatibility", () => {
  const cases = [
    ["/tmp/sweep/000/workspace-concurrency-3.asb-agent-runtime/a", "<asb-workspace>/a"],
    ["relative/workspace/file", "<asb-workspace>/file"],
    ["/tmp/workspaceSuffix", "/tmp/workspaceSuffix"],
    ["/asb-snapshot-storage/snapshot/filesystem/workspace/a", "/asb-snapshot-storage/snapshot/filesystem/workspace/a"],
  ];
  for (const [input, expected] of cases) {
    assert.deepEqual(JSON.parse(normalizeWorkspacePaths(JSON.stringify({ message: input }))), { message: expected });
  }
});

test("recorded provider responses preserve unrelated workspace-like paths", { timeout: 10000 }, async (t) => {
  const cases = [
    "/tmp/sweep/000/workspace-concurrency-3.asb-agent-runtime/a",
    "relative/workspace/file", "/tmp/workspace", "/tmp/workspaceSuffix",
    "/a//b/workspace", "foo/workspace-concurrency-1.suffix/bar/workspace-concurrency-2tail",
    "workspace-concurrency-1", "/workspace-concurrency-1",
    "x/workspace-concurrency-2/x/workspace-concurrency-3tail",
    "x/workspace-concurrency-2//y/workspace-concurrency-3tail",
    "/a/workspace/file /b/workspace/after", "/a/é/workspace",
    "a/workspace\\literal", "a/workspace\nnext",
  ];
  let index = 0;
  const { port, stop } = await fixture(t, (_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ message: cases[index++] }));
  });
  for (const _ of cases) assert.equal((await request(port)).status, 200);
  const { value, bytes } = await stop();
  assert.equal(value.complete, true);
  const entries = bytes.toString().trim().split("\n").map(JSON.parse);
  cases.forEach((sample, offset) => {
    assert.deepEqual(JSON.parse(entries[offset].response.body), { message: sample }, sample);
  });
});

test("recorded responses preserve the selected workspace", { timeout: 10000 }, async (t) => {
  const workspace = "/tmp/sweep/000/workspace-concurrency-3";
  const cases = [
    [workspace, "<asb-workspace>"],
    [`${workspace}/src/file`, "<asb-workspace>/src/file"],
    [`${workspace}.asb-agent-runtime/a`, `${workspace}.asb-agent-runtime/a`],
    [`${workspace}-other/a`, `${workspace}-other/a`],
    ["/unrelated/workspace/src/file", "/unrelated/workspace/src/file"],
  ];
  let index = 0;
  const { port, stop } = await fixture(t, (_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ message: cases[index++][0] }));
  }, { workspace });
  for (const _ of cases) assert.equal((await request(port)).status, 200);
  const { value, bytes } = await stop();
  assert.equal(value.complete, true);
  const entries = bytes.toString().trim().split("\n").map(JSON.parse);
  cases.forEach(([sample, expected], offset) => {
    assert.deepEqual(JSON.parse(entries[offset].response.body), { message: sample }, sample);
  });
});

test("empty and probe-only captures have honest complete/zero-inference observations", { timeout: 10000 }, async (t) => {
  for (const probe of [false, true]) {
    const { port, stop } = await fixture(t, (_req, res) => res.end("{}"));
    if (probe) assert.equal((await request(port, "/health", null)).status, 200);
    const { value } = await stop();
    assert.equal(value.complete, true);
    assert.equal(value.accepted, Number(probe));
    assert.equal(value.inference_requests, 0);
    assert.equal(value.recorded, Number(probe));
  }
});

test("embedding exchanges are retained without being counted as language-model inference", { timeout: 10000 }, async (t) => {
  const { port, stop } = await fixture(t, (_req, res) => res.end("{}"));
  assert.equal((await request(port, "/v1/embeddings?fixture=1", JSON.stringify({ model: "text-embedding-3-small", input: ["memory"] }))).status, 200);
  assert.equal((await request(port, "/v1/embeddings/", JSON.stringify({ model: "fixture-embedding", input: ["memory"] }))).status, 200);
  assert.equal((await request(port, "/v1/messages", JSON.stringify({ model: "fixture", messages: [] }))).status, 200);
  const { value, bytes } = await stop();
  assert.equal(value.complete, true);
  assert.equal(value.recorded, 3);
  assert.equal(value.inference_requests, 1);
  const entries = bytes.toString().trim().split("\n").map(JSON.parse);
  assert.equal(entries[0].request.url, "/v1/embeddings?fixture=1");
  assert.equal(entries[1].request.url, "/v1/embeddings/");
  assert.equal(entries[2].request.url, "/v1/messages");
});

test("complete HTTP error is captured and grace drains concurrent already-admitted responses", { timeout: 10000 }, async (t) => {
  let seen = 0;
  let announce;
  const admitted = new Promise((resolve) => { announce = resolve; });
  const { port, stop } = await fixture(t, (_req, res) => {
    if (++seen === 2) announce();
    setTimeout(() => { res.writeHead(500, { "content-type": "application/json" }); res.end('{"error":"fixture"}'); }, 80);
  });
  const first = request(port);
  const second = request(port);
  await admitted;
  const stopped = stop();
  assert.equal((await first).status, 500);
  assert.equal((await second).status, 500);
  const { value, bytes } = await stopped;
  assert.equal(value.complete, true);
  assert.equal(value.completed, 2);
  assert.equal(value.inference_requests, 2);
  assert.equal(bytes.toString().trim().split("\n").length, 2);
});

test("partial upload is accounted before its body and forced shutdown remains bounded", { timeout: 10000 }, async (t) => {
  const { port, stop } = await fixture(t, () => assert.fail("partial upload must never reach upstream"));
  const socket = net.connect(port, "127.0.0.1");
  socket.on("error", () => {});
  t.after(() => socket.destroy());
  await new Promise((resolve) => socket.once("connect", resolve));
  socket.write("POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000\r\n\r\nx");
  await delay(50);
  const { value } = await stop();
  assert.equal(value.accepted, 1);
  assert.equal(value.failed, 1);
  assert.equal(value.recorded, 0);
  assert.equal(value.complete, false);
});

for (const scenario of ["aborted-upstream", "hanging-upstream", "disconnected-downstream"]) {
  test(`transport interruption: ${scenario}`, { timeout: 10000 }, async (t) => {
    let announce;
    const admitted = new Promise((resolve) => { announce = resolve; });
    const { port, stop } = await fixture(t, (_req, res) => {
      announce();
      if (scenario === "aborted-upstream") {
        res.writeHead(200, { "content-length": "1000" });
        res.write("short");
        res.destroy();
      } else if (scenario === "disconnected-downstream") {
        setTimeout(() => res.end("full upstream response"), 60);
      }
    });
    const req = http.request({ host: "127.0.0.1", port, path: "/v1/responses", method: "POST" });
    req.on("error", () => {});
    req.on("response", (res) => { res.on("error", () => {}); res.resume(); });
    req.end("{}");
    await admitted;
    if (scenario === "disconnected-downstream") req.destroy();
    const { value } = await stop();
    assert.equal(value.accepted, 1);
    assert.equal(value.failed, 1);
    assert.equal(value.complete, false);
    assert.equal(value.recorded, scenario === "disconnected-downstream" ? 1 : 0);
  });
}
