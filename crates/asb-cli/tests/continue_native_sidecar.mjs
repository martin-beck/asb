import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { encodeSidecar, decodeSidecar } from '../assets/scripts/native-tool-sidecar.mjs';
const hash = value => createHash('sha256').update(value).digest('hex');
const exchanges = [
  {ordinal:1,request:{tool:'Fetch',args:{url:'fixture'}},outcome:{ok:true,value:'answer'}},
  {ordinal:0,request:{tool:'Read',args:{path:'fixture'}},outcome:{ok:false,name:'Error',message:'failed'}},
];
const program=hash('fixture-program'), workspace=hash('verified-fixture-workspace');
const encoded = encodeSidecar(exchanges,program,workspace);
const decoded = decodeSidecar(encoded);
assert.deepEqual(decoded.exchanges,exchanges);
assert.equal(decoded.programSha256,program);
assert.equal(decoded.workspaceEffectSha256,workspace);
const events = encoded.toString('utf8').trimEnd().split('\n').map(JSON.parse);
const mutated = structuredClone(events);mutated[3].request_sha256 = '0'.repeat(64);
assert.throws(()=>decodeSidecar(Buffer.from(mutated.map(JSON.stringify).join('\n')+'\n')),/verification/);
const reordered = structuredClone(events);[reordered[2],reordered[3]]=[reordered[3],reordered[2]];
assert.throws(()=>decodeSidecar(Buffer.from(reordered.map(JSON.stringify).join('\n')+'\n')),/order/);
assert.throws(()=>encodeSidecar([],program,workspace),/count/);
assert.throws(()=>encodeSidecar(exchanges,program,'invalid'),/digests/);
assert.throws(()=>encodeSidecar([{ordinal:0,request:{tool:'Read'},outcome:{ok:true,value:'a'.repeat(600000)}}],program,workspace),/bounds/);
assert.throws(()=>decodeSidecar(encoded.subarray(0,-1)),/framing/);
console.log('Continue native sidecar codec checks passed');
