import assert from 'node:assert/strict';
import {mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {NativeToolBroker} from '../assets/scripts/native-tool-broker.mjs';
import {createOpenCodeNativePlugin} from '../assets/scripts/opencode-native-plugin.mjs';

const dir=await mkdtemp(join(tmpdir(),'asb-opencode-native-'));
const socket=join(dir,'broker.sock');
try {
  const recorder=new NativeToolBroker('record');
  await recorder.listen(socket);
  const record=createOpenCodeNativePlugin({mode:'record',socket});
  const call={tool:'bash',sessionID:'parent',callID:'1'};
  const child={tool:'read',sessionID:'child',callID:'1'};
  await record['tool.execute.before'](call,{args:{command:'printf x',timeout:1234}});
  await record['tool.execute.before'](child,{args:{filePath:'a'}});
  await record['tool.execute.before']({tool:'task',sessionID:'parent',callID:'2'},{args:{}});
  await record['tool.execute.after'](child,{title:'read',output:'content',metadata:{}});
  await record['tool.execute.after'](call,{title:'shell',output:'x',metadata:{exit:0}});
  const events=await recorder.finish();
  assert.equal(events.length,2);
  const effects=[];
  const broker=new NativeToolBroker('replay',events,async event=>effects.push(event.ordinal));
  await broker.listen(socket);
  const definitions={bash:{description:'shell',schema:{type:'object',properties:{command:{type:'string'},timeout:{type:'integer'}},required:['command']}},read:{description:'read',schema:{type:'object',properties:{filePath:{type:'string'}}}},task:{description:'delegate',schema:{type:'object'}}};
  const replay=createOpenCodeNativePlugin({mode:'replay',socket,definitions});
  assert.equal(replay.tool.task,undefined);
  const schema={}; await replay['tool.definition']({toolID:'bash'},schema);
  assert.deepEqual(schema.jsonSchema,definitions.bash.schema);
  const shell=replay.tool.bash.execute({command:'printf x',timeout:1234});
  const read=replay.tool.read.execute({filePath:'a'});
  assert.deepEqual(await read,{title:'read',output:'content',metadata:{}});
  assert.deepEqual(await shell,{title:'shell',output:'x',metadata:{exit:0}});
  await broker.finish();
  assert.deepEqual(effects,[1,0]);
} finally { await rm(dir,{recursive:true,force:true}); }
