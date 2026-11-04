import { createHash } from 'node:crypto';
import { encodeSidecar } from '../assets/scripts/continue-native-sidecar.mjs';
const hash = value => createHash('sha256').update(value).digest('hex');
const exchanges = [
  {ordinal:1,request:{tool:'Fetch',args:{url:'fixture'}},outcome:{ok:true,value:'answer'}},
  {ordinal:0,request:{tool:'Read',args:{path:'fixture'}},outcome:{ok:false,name:'Error',message:'failed'}},
];
process.stdout.write(encodeSidecar(exchanges,hash('fixture-program'),hash('verified-fixture-workspace')));
