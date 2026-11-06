import { createHash } from 'node:crypto';
import { encodeSidecar, encodePendingSidecar } from '../assets/scripts/native-tool-sidecar.mjs';
const hash = value => createHash('sha256').update(value).digest('hex');
const exchanges = [
  {ordinal:1,request:{tool:'Fetch',args:{url:'fixture'}},outcome:{ok:true,value:'answer'}},
  {ordinal:0,request:{tool:'Read',args:{path:'fixture'}},outcome:{ok:false,name:'Error',message:'failed'}},
];
if (process.argv.includes('--sequence')) {
  exchanges[0].snapshot_sequence = 3;
  exchanges[1].snapshot_sequence = 2;
}
const agent=['qwen-code','pi','opencode','claude-code'].find(name => process.argv.includes(`--${name}`)) || 'continue';
process.stdout.write(process.argv.includes('--pending') ? encodePendingSidecar(exchanges,hash('fixture-program'),agent) : encodeSidecar(exchanges,hash('fixture-program'),hash('verified-fixture-workspace'),agent));
