import assert from 'node:assert/strict';
import {installQwenToolContext} from '../assets/scripts/qwen-native-context.mjs';
const seen=[];
const previous=globalThis.fetch;
globalThis.fetch=async (input,init)=>{const request=new Request(input,init);seen.push({url:request.url,tag:request.headers.get('x-asb-native-tool')});return new Response('ok');};
try {
  const context=installQwenToolContext('http://127.0.0.1:9000/v1');
  globalThis.fetch=context.wrapFetch(globalThis.fetch);
  const invoke=context.invokeRecorded;
  await Promise.all([invoke(7,async()=>{await new Promise(r=>setTimeout(r,5));await fetch('http://127.0.0.1:9000/v1/chat/completions');await fetch('http://127.0.0.1:9001/v1/chat/completions');}),invoke(9,()=>fetch('http://127.0.0.1:9000/v1/messages'))]);
  await fetch('http://127.0.0.1:9000/v1/chat/completions');
  assert.equal(seen.find(row=>row.url.endsWith('/v1/messages')).tag,'v1:qwen-code:9');
  assert.equal(seen.filter(row=>row.url.includes(':9000') && row.url.endsWith('/chat/completions'))[0].tag,'v1:qwen-code:7');
  assert.equal(seen.find(row=>row.url.includes(':9001')).tag,null);
  assert.equal(seen.at(-1).tag,null);
  assert.throws(()=>invoke(-1,()=>{}),TypeError);
} finally {globalThis.fetch=previous;}
