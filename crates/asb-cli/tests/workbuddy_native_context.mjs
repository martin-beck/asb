import assert from 'node:assert/strict';
import {installWorkBuddyToolContext} from '../assets/scripts/workbuddy-native-context.mjs';
const context=installWorkBuddyToolContext('http://127.0.0.1:1234/v1');
const observed=[];
const wrapped=context.wrapFetch(async (url,init)=>{observed.push({url,header:new Headers(init?.headers).get('x-asb-native-tool')});return {ok:true}});
await wrapped('http://127.0.0.1:1234/v1/chat',{});
await context.invokeRecorded(7,async()=>{await wrapped('http://127.0.0.1:1234/v1/chat',{});await wrapped('https://example.com/',{});});
assert.deepEqual(observed.map(x=>x.header),[null,'v1:workbuddy:7',null]);
const requests=[];
const request=context.wrapRequest((...args)=>requests.push(args),'http:');
await context.invokeRecorded(9,async()=>{
  request('http://127.0.0.1:1234/v1/chat', {method:'POST'});
  request({hostname:'127.0.0.1',port:1234,path:'/v1/chat'});
  request('https://example.com/', {});
});
assert.equal(requests[0][1].headers['x-asb-native-tool'],'v1:workbuddy:9');
assert.equal(requests[1][0].headers['x-asb-native-tool'],'v1:workbuddy:9');
assert.equal(requests[2][1].headers,undefined);
