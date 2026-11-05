import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, existsSync, unlinkSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { startContinue, publishPending } from '../assets/scripts/continue-native-supervisor.mjs';
import { createHash } from 'node:crypto';

const root = mkdtempSync(join(tmpdir(), 'asb-continue-supervisor-'));
try {
  const bundle = join(root, 'bundle.mjs'), socket = join(root, 'broker.sock');
  const effect = join(root, 'live-tool-called');
  const input = join(root, 'read-input.txt');
  writeFileSync(input, 'read-state fixture');
  writeFileSync(bundle, `import * as f from 'node:fs';
const reads=new Set();function remember(e){reads.add(e)}
function read(s){let full=f.realpathSync(s),content=f.readFileSync(full,'utf8');return remember(full),\`Content of \${s}: \${content}\`}
function guard(){cE.getInstance().getCurrentSession();let a="";if(!a)throw new Error("No active session found");return a}
async function dispatch(e,r){return await e.tool.run(e.preprocessResult?.args??e.arguments,r)}
const readResult=await dispatch({name:"Read",displayName:"Read",arguments:{filepath:process.env.FIXTURE_INPUT},tool:{run:async args=>read(args.filepath)}},{});
if(!reads.has(f.realpathSync(process.env.FIXTURE_INPUT))||!readResult.includes('read-state fixture'))throw new Error('Read state was not retained');
const result=await dispatch({name:'Write',arguments:{path:'fixture'},tool:{run:async()=>{f.writeFileSync(process.env.FIXTURE_EFFECT,'executed');return 'result'}}},{});
if(result!=='result')process.exit(2);
`);
  const config = { node: process.execPath, cli: bundle, args: [], cwd: root,
    env: { PATH: process.env.PATH || '', FIXTURE_EFFECT: effect, FIXTURE_INPUT: input, ASB_CONTINUE_MARKER: join(root, 'stale.sock') },
    bundle, socket, stdio: ['ignore','ignore','pipe'] };
  const record = await startContinue({ ...config, mode: 'record' });
  let recordErrors=''; record.child.stderr.on('data',bytes=>{recordErrors=(recordErrors+bytes).slice(0,4096)});
  assert.deepEqual(await record.exited, { code: 0, signal: null }, recordErrors);
  const events = record.broker.session.preview();
  assert.equal(events.length, 2);
  const artifact = join(root, 'pending.jsonl');
  const programSha256 = createHash('sha256').update(readFileSync(bundle)).digest('hex');
  await assert.rejects(publishPending(record, { programSha256, destination:artifact, treeQuiesced:false }), /quiescence/);
  await publishPending(record, { programSha256, destination:artifact, treeQuiesced:true });
  assert.doesNotMatch(readFileSync(artifact,'utf8'), /workspace_effect/);
  assert.equal(existsSync(effect), true);
  unlinkSync(effect);
  const replay = await startContinue({ ...config, mode: 'replay', events });
  let replayErrors=''; replay.child.stderr.on('data',bytes=>{replayErrors=(replayErrors+bytes).slice(0,4096)});
  assert.deepEqual(await replay.exited, { code: 0, signal: null }, replayErrors);
  await replay.broker.finish();
  assert.equal(existsSync(effect), false, 'replay must not invoke the live tool');
  await assert.rejects(startContinue({ ...config, mode: 'invalid' }), /configuration/);
  console.log('Continue supervisor lifecycle checks passed');
} finally { rmSync(root, { recursive: true, force: true }); }
