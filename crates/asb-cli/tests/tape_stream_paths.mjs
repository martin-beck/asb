import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { once } from 'node:events';
import http from 'node:http';

const script = new URL('../assets/scripts/llmtape.mjs', import.meta.url).pathname;
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function start(args, ready, workspace) {
  const child = spawn(process.execPath, [script, ...args], {env:{...process.env,ASB_TAPE_READY_FILE:ready,ASB_TAPE_WORKSPACE:workspace},stdio:'ignore'});
  for (let attempt=0; attempt<200; attempt++) {
    if (child.exitCode !== null) throw new Error(`proxy exited: ${child.exitCode}`);
    try { return {child,port:JSON.parse(await readFile(ready,'utf8')).port}; }
    catch (error) { if (error.code !== 'ENOENT') throw error; await pause(10); }
  }
  child.kill('SIGTERM'); throw new Error('proxy did not become ready');
}
async function stop(child) {
  if (child.exitCode !== null) return;
  const done=once(child,'exit'); child.kill('SIGTERM'); await done;
}
function field(body, read) {
  return body.split('\n').filter((line)=>line.startsWith('data: {')).map((line)=>read(JSON.parse(line.slice(6)).choices[0].delta)).join('');
}

test('record and replay preserve split guest paths and remap split host paths', async () => {
  const root=await mkdtemp(join(tmpdir(),'asb-sse-paths-'));
  const recordWorkspace='/srv/record/workspace-concurrency-1';
  const replayWorkspace='/srv/replay/workspace-concurrency-2';
  const guest='/asb-snapshot-storage/snapshot/filesystem/workspace/a';
  const literal=' 2022-12-16T00:00:00Z 1234567890123 123e4567-e89b-12d3-a456-426614174000 /unrelated/project/workspace/file '+recordWorkspace+'-backup/file';
  const chunks=['/asb-snapshot-storage/snapshot/files','ystem/workspace/a and /srv/record/workspace-con','currency-1/b'+literal];
  const body=chunks.map((value)=>`data: ${JSON.stringify({choices:[{index:0,delta:{reasoning:value,tool_calls:[{index:0,function:{arguments:value}}]}}]})}\n\n`).join('')+'data: [DONE]\n\n';
  const upstream=http.createServer((_request,response)=>{response.writeHead(200,{'content-type':'text/event-stream'});response.end(body);});
  await new Promise((resolve)=>upstream.listen(0,'127.0.0.1',resolve));
  let record,replay;
  try {
    const cassette=join(root,'capture.jsonl');
    record=await start(['record','--port','0','--upstream',`http://127.0.0.1:${upstream.address().port}`,'--out',cassette],join(root,'record.ready'),recordWorkspace);
    const options={method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:'fixture',messages:[]})};
    const live=await fetch(`http://127.0.0.1:${record.port}/v1/chat/completions`,options);
    const liveBody=await live.text();
    assert.equal(field(liveBody, d=>d.reasoning),`${guest} and ${recordWorkspace}/b${literal}`);
    assert.equal(field(liveBody, d=>d.tool_calls[0].function.arguments),`${guest} and ${recordWorkspace}/b${literal}`);
    await stop(record.child); record=null;
    await new Promise((resolve)=>upstream.close(resolve));
    replay=await start(['replay','--port','0','--cassette',cassette],join(root,'replay.ready'),replayWorkspace);
    const result=await fetch(`http://127.0.0.1:${replay.port}/v1/chat/completions`,options);
    assert.equal(result.status,200);
    const replayBody=await result.text();
    assert.equal(field(replayBody, d=>d.reasoning),`${guest} and ${replayWorkspace}/b${literal}`);
    assert.equal(field(replayBody, d=>d.tool_calls[0].function.arguments),`${guest} and ${replayWorkspace}/b${literal}`);
  } finally {
    if (record) await stop(record.child);
    if (replay) await stop(replay.child);
    if (upstream.listening) await new Promise((resolve)=>upstream.close(resolve));
    await rm(root,{recursive:true,force:true});
  }
});

test('replay rejects changed literal dates, numbers and UUIDs in task data', async () => {
  const root=await mkdtemp(join(tmpdir(),'asb-timestamp-'));
  const upstream=http.createServer((_request,response)=>{response.writeHead(200,{'content-type':'application/json'});response.end('{"ok":true}');});
  await new Promise((resolve)=>upstream.listen(0,'127.0.0.1',resolve));
  let record,replay;
  const original={model:'fixture',messages:[{role:'user',content:'Use 2022-12-16T00:00:00Z, 1700000000000, and 123e4567-e89b-12d3-a456-426614174000, plus Jan 12 12:34.'},{role:'assistant',tool_calls:[{id:'call-fixture',type:'function',function:{name:'bash',arguments:'{"command":"echo 2022-12-16T00:00:00Z"}'}}]}]};
  const options=(body)=>({method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});
  try {
    const cassette=join(root,'capture.jsonl');
    record=await start(['record','--port','0','--upstream',`http://127.0.0.1:${upstream.address().port}`,'--out',cassette],join(root,'record.ready'),'/srv/record/workspace');
    assert.equal((await fetch(`http://127.0.0.1:${record.port}/v1/chat/completions`,options(original))).status,200);
    await stop(record.child); record=null;
    await new Promise((resolve)=>upstream.close(resolve));
    replay=await start(['replay','--port','0','--cassette',cassette],join(root,'exact.ready'),'/srv/replay/workspace');
    assert.equal((await fetch(`http://127.0.0.1:${replay.port}/v1/chat/completions`,options(original))).status,200);
    await stop(replay.child); replay=null;
    for (const [index,change] of [
      (v)=>{v.messages[0].content=v.messages[0].content.replace('2022','2023');},
      (v)=>{v.messages[0].content=v.messages[0].content.replace('1700000000000','1800000000000');},
      (v)=>{v.messages[0].content=v.messages[0].content.replace('123e4567','223e4567');},
      (v)=>{v.messages[0].content=v.messages[0].content.replace('Jan 12 12:34','Jan 13 12:34');},
      (v)=>{v.messages[1].tool_calls[0].function.arguments=v.messages[1].tool_calls[0].function.arguments.replace('2022','2023');},
    ].entries()) {
      const modified=structuredClone(original); change(modified);
      replay=await start(['replay','--port','0','--cassette',cassette],join(root,`replay-${index}.ready`),'/srv/replay/workspace');
      assert.equal((await fetch(`http://127.0.0.1:${replay.port}/v1/chat/completions`,options(modified))).status,409);
      await stop(replay.child); replay=null;
    }
  } finally {
    if (record) await stop(record.child);
    if (replay) await stop(replay.child);
    if (upstream.listening) await new Promise((resolve)=>upstream.close(resolve));
    await rm(root,{recursive:true,force:true});
  }
});

test('Anthropic streamed tool arguments keep unrelated workspace paths', async () => {
  const root=await mkdtemp(join(tmpdir(),'asb-anthropic-paths-'));
  const recordWorkspace='/srv/record/workspace';
  const replayWorkspace='/srv/replay/workspace';
  const pieces=['{"command":"cat /srv/record/work','space/a /other/project/workspace/b"}'];
  const body=pieces.map(part=>`data: ${JSON.stringify({type:'content_block_delta',index:0,delta:{type:'input_json_delta',partial_json:part}})}\n\n`).join('')+
    `data: ${JSON.stringify({type:'content_block_stop',index:0})}\n\n`;
  const upstream=http.createServer((_request,response)=>{response.writeHead(200,{'content-type':'text/event-stream'});response.end(body);});
  await new Promise(resolve=>upstream.listen(0,'127.0.0.1',resolve));
  let record,replay;
  const options={method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:'fixture',messages:[]})};
  try {
    const cassette=join(root,'capture.jsonl');
    record=await start(['record','--port','0','--upstream',`http://127.0.0.1:${upstream.address().port}`,'--out',cassette],join(root,'record.ready'),recordWorkspace);
    assert.equal(await (await fetch(`http://127.0.0.1:${record.port}/v1/messages`,options)).text(),body);
    await stop(record.child); record=null;
    await new Promise(resolve=>upstream.close(resolve));
    replay=await start(['replay','--port','0','--cassette',cassette],join(root,'replay.ready'),replayWorkspace);
    const response=await fetch(`http://127.0.0.1:${replay.port}/v1/messages`,options);
    assert.equal(response.status,200);
    const reconstructed=(await response.text()).split('\n').filter(line=>line.startsWith('data: {')).map(line=>JSON.parse(line.slice(6)).delta?.partial_json || '').join('');
    assert.equal(JSON.parse(reconstructed).command,`cat ${replayWorkspace}/a /other/project/workspace/b`);
  } finally {
    if (record) await stop(record.child);
    if (replay) await stop(replay.child);
    if (upstream.listening) await new Promise(resolve=>upstream.close(resolve));
    await rm(root,{recursive:true,force:true});
  }
});

test('non-streaming JSON handles escaped workspace paths without touching siblings', async () => {
  const root=await mkdtemp(join(tmpdir(),'asb-json-paths-'));
  const recordWorkspace='/srv/record/workspace';
  const replayWorkspace='/srv/replay/workspace';
  const payload={choices:[{message:{content:`${recordWorkspace}/a /other/project/workspace/b ${recordWorkspace}-backup/c`}}]};
  const encoded=JSON.stringify(payload).replace(/}$/, ',\"counter\":9007199254740993}').replaceAll('/', '\\/');
  const upstream=http.createServer((_request,response)=>{response.writeHead(200,{'content-type':'application/json'});response.end(encoded);});
  await new Promise(resolve=>upstream.listen(0,'127.0.0.1',resolve));
  let record,replay;
  const options={method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:'fixture',messages:[]})};
  try {
    const cassette=join(root,'capture.jsonl');
    record=await start(['record','--port','0','--upstream',`http://127.0.0.1:${upstream.address().port}`,'--out',cassette],join(root,'record.ready'),recordWorkspace);
    const liveText=await (await fetch(`http://127.0.0.1:${record.port}/v1/chat/completions`,options)).text();
    assert.equal(JSON.parse(liveText).choices[0].message.content,payload.choices[0].message.content);
    await stop(record.child); record=null;
    await new Promise(resolve=>upstream.close(resolve));
    replay=await start(['replay','--port','0','--cassette',cassette],join(root,'replay.ready'),replayWorkspace);
    const replayText=await (await fetch(`http://127.0.0.1:${replay.port}/v1/chat/completions`,options)).text();
    assert.ok(replayText.includes('9007199254740993'));
    const value=JSON.parse(replayText);
    assert.equal(value.choices[0].message.content,`${replayWorkspace}/a /other/project/workspace/b ${recordWorkspace}-backup/c`);
  } finally {
    if (record) await stop(record.child);
    if (replay) await stop(replay.child);
    if (upstream.listening) await new Promise(resolve=>upstream.close(resolve));
    await rm(root,{recursive:true,force:true});
  }
});
