import assert from 'node:assert/strict';
import {runClineNativeSession} from '../assets/scripts/cline-native-session.mjs';

let disposed = 0, captured;
const tool = {snapshot: {agentId: 'child'}, toolCall: {toolCallId: 'call'}, tool: {name: 'read_files'}, input: {files: ['x']}};
let failChild = false;
const coreModule = {
  getClineDefaultSystemPrompt: ({rootPath, platform}) => `${rootPath}:${platform}`,
  ClineCore: {async create(options) {
    assert.equal(options.backendMode, 'local');
    assert.equal(await options.capabilities.toolExecutors.askQuestion('question', ['first', 'second']), 'first');
    await assert.rejects(options.capabilities.toolExecutors.askQuestion('question', []), /no available answer/);
    return {
      async start({config}) { captured = config; return {sessionId: 'session'}; },
      async send({sessionId, prompt}) {
        assert.equal(sessionId, 'session'); assert.equal(prompt, 'task');
        if (failChild) {
          // A parent can catch a delegated failure and still report success.
          try { await captured.hooks.beforeTool(tool); } catch { /* simulated parent */ }
        }
        return {finishReason: 'completed'};
      },
      async dispose() { disposed++; },
    };
  }},
};
const config = {coreModule, transport: {}, mode: 'record', workspace: '/fixture', provider: 'fixture', model: 'model', baseUrl: 'http://127.0.0.1', apiKey: 'key', prompt: 'task', maxIterations: 200};
assert.equal((await runClineNativeSession(config)).finishReason, 'completed');
assert.equal(captured.hookErrorMode, 'throw');
assert.equal(captured.maxIterations, 200);
assert.equal(captured.enableSpawnAgent && captured.enableAgentTeams, true);
assert.equal(captured.systemPrompt, `/fixture:${process.platform}`);
failChild = true;
await assert.rejects(runClineNativeSession({...config, transport: {start: async () => { throw new Error('broker failed'); }}}), /capture or replay failed/);
assert.equal(disposed, 2);
await assert.rejects(runClineNativeSession({...config, maxIterations: 0}), /iteration limit/);
console.log('Cline native session: fail-closed hooks and delegated failure settlement passed');
