import assert from 'node:assert/strict';
import vm from 'node:vm';
import { transformDispatch, transformHeadlessSubagent } from '../assets/scripts/continue-dispatch-transform.mjs';

const source = 'async function dispatch(e,r){return await e.tool.run(e.preprocessResult?.args??e.arguments,r)}; dispatch';
const sandbox = {Symbol, globalThis: null};
sandbox.globalThis = sandbox;
const seen = [];
sandbox[Symbol.for('asb.continue.nativeDispatch.v1')] = async (request, context, invoke) => {
  seen.push({ request, context });
  return invoke();
};
const dispatch = vm.runInNewContext(transformDispatch(source), sandbox);
const tool = { run: async (args, ctx) => `${args.path}:${ctx.id}` };
assert.deepEqual(await Promise.all([
  dispatch({ tool, arguments: { path: 'first' } }, { id: 1 }),
  dispatch({ tool, arguments: { path: 'ignored' }, preprocessResult: { args: { path: 'second' } } }, { id: 2 }),
]), ['first:1', 'second:2']);
assert.equal(seen.length, 2);
const failure = new Error('fixture error');
await assert.rejects(dispatch({ tool: { run: async () => { throw failure; } }, arguments: {} }, {}), error => error === failure);
assert.throws(() => transformDispatch('no supported dispatch'), /found 0/);
assert.throws(() => transformDispatch(`${source};${source}`), /found 2/);
assert.throws(() => transformDispatch(null), /must be text/);
const guard = 'cE.getInstance().getCurrentSession();let a="";if(!a)throw new Error("No active session found"); a';
const withSession = { cE: { getInstance: () => ({ getCurrentSession: () => ({ sessionId: 'fixture-session' }) }) } };
assert.equal(vm.runInNewContext(transformHeadlessSubagent(guard), withSession), 'fixture-session');
assert.throws(() => transformHeadlessSubagent('no guard'), /unsupported or ambiguous/);
assert.throws(() => transformHeadlessSubagent(`${guard};${guard}`), /unsupported or ambiguous/);
console.log('Continue dispatch transform checks passed');

// Disposable local qualification may additionally check the exact installed
// client. CI remains offline and does not require a globally installed agent.
if (process.env.ASB_TEST_CONTINUE_BUNDLE) {
  const { readFileSync } = await import('node:fs');
  const installed = readFileSync(process.env.ASB_TEST_CONTINUE_BUNDLE, 'utf8');
  assert.match(transformDispatch(installed), /asb\.continue\.nativeDispatch\.v1/);
  assert.match(transformHeadlessSubagent(installed), /if\(!a\)a=cE\.getInstance\(\)\.getCurrentSession\(\)\.sessionId/);
  console.log('Installed Continue dispatch transform passed');
}

// Verify Node's real module loader transforms the named module in both the
// parent and a Node subprocess, without modifying either program on disk.
{
  const { mkdtempSync, writeFileSync, readFileSync, rmSync } = await import('node:fs');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');
  const { spawnSync } = await import('node:child_process');
  const { pathToFileURL } = await import('node:url');
  const root = mkdtempSync(join(tmpdir(), 'asb-continue-dispatch-'));
  try {
    const bundle = join(root, 'bundle.mjs');
    const bootstrap = join(root, 'bootstrap.mjs');
    const log = join(root, 'dispatch.log');
    const moduleUrl = new URL('../assets/scripts/continue-dispatch-hook.mjs', import.meta.url).href;
    writeFileSync(bundle, `import { spawnSync } from 'node:child_process';\nasync function dispatch(e,r){return await e.tool.run(e.preprocessResult?.args??e.arguments,r)}\nawait dispatch({tool:{run:async x=>x},arguments:{id:process.argv[2]||'parent'}},{});\nif(!process.argv[2]){const r=spawnSync(process.execPath,[process.argv[1],'child'],{env:process.env,stdio:'inherit'});if(r.status!==0)process.exit(r.status||1)}\n`);
    writeFileSync(bootstrap, `import {appendFileSync} from 'node:fs';\nimport {installDispatchHook} from ${JSON.stringify(moduleUrl)};\ninstallDispatchHook(process.env.ASB_TEST_BUNDLE,async(request,_context,invoke)=>{appendFileSync(process.env.ASB_TEST_LOG,request.arguments.id+'\\n');return invoke()});\n`);
    const result = spawnSync(process.execPath, ['--import', pathToFileURL(bootstrap).href, bundle], {
      encoding: 'utf8',
      env: { ...process.env, NODE_OPTIONS: `--import=${pathToFileURL(bootstrap).href}`, ASB_TEST_BUNDLE: bundle, ASB_TEST_LOG: log },
    });
    // The explicit --import is only passed to the parent; NODE_OPTIONS makes
    // it available to the child and Node deduplicates the identical import.
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(readFileSync(log, 'utf8').trim().split('\n'), ['parent', 'child']);
  } finally { rmSync(root, { recursive: true, force: true }); }
}
