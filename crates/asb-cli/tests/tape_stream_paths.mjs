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
function reasoning(body) {
  return body.split('\n').filter((line)=>line.startsWith('data: {')).map((line)=>JSON.parse(line.slice(6)).choices[0].delta.reasoning).join('');
}

test('record and replay preserve split guest paths and remap split host paths', async () => {
  const root=await mkdtemp(join(tmpdir(),'asb-sse-paths-'));
  const recordWorkspace='/srv/record/workspace-concurrency-1';
  const replayWorkspace='/srv/replay/workspace-concurrency-2';
  const guest='/asb-snapshot-storage/snapshot/filesystem/workspace/a';
  const chunks=['/asb-snapshot-storage/snapshot/files','ystem/workspace/a and /srv/record/workspace-con','currency-1/b'];
  const body=chunks.map((value)=>`data: ${JSON.stringify({choices:[{index:0,delta:{reasoning:value}}]})}\n\n`).join('')+'data: [DONE]\n\n';
  const upstream=http.createServer((_request,response)=>{response.writeHead(200,{'content-type':'text/event-stream'});response.end(body);});
  await new Promise((resolve)=>upstream.listen(0,'127.0.0.1',resolve));
  let record,replay;
  try {
    const cassette=join(root,'capture.jsonl');
    record=await start(['record','--port','0','--upstream',`http://127.0.0.1:${upstream.address().port}`,'--out',cassette],join(root,'record.ready'),recordWorkspace);
    const options={method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:'fixture',messages:[]})};
    const live=await fetch(`http://127.0.0.1:${record.port}/v1/chat/completions`,options);
    assert.equal(reasoning(await live.text()),`${guest} and ${recordWorkspace}/b`);
    await stop(record.child); record=null;
    await new Promise((resolve)=>upstream.close(resolve));
    replay=await start(['replay','--port','0','--cassette',cassette],join(root,'replay.ready'),replayWorkspace);
    const result=await fetch(`http://127.0.0.1:${replay.port}/v1/chat/completions`,options);
    assert.equal(result.status,200);
    assert.equal(reasoning(await result.text()),`${guest} and ${replayWorkspace}/b`);
  } finally {
    if (record) await stop(record.child);
    if (replay) await stop(replay.child);
    if (upstream.listening) await new Promise((resolve)=>upstream.close(resolve));
    await rm(root,{recursive:true,force:true});
  }
});
