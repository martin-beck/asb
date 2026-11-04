// Cline's core API exposes the same native tools as its CLI and allows hook
// failures to stop execution. The adapter supplies the broker-backed hooks.
import {createClineNativeHooks} from './cline-native-hooks.mjs';

export async function runClineNativeSession({coreModule, transport, mode, workspace, provider, model, baseUrl, apiKey, prompt, maxIterations, onEvent}) {
  if (!coreModule?.ClineCore || typeof coreModule.getClineDefaultSystemPrompt !== 'function' ||
      !['record', 'replay'].includes(mode) || !workspace || !provider || !model || !baseUrl || !prompt)
    throw new TypeError('Cline native session configuration is incomplete');
  if (maxIterations !== undefined && (!Number.isSafeInteger(maxIterations) || maxIterations < 1))
    throw new TypeError('Cline native session iteration limit is invalid');
  const native = createClineNativeHooks(mode, transport);
  const core = await coreModule.ClineCore.create({
    backendMode: 'local',
    clientName: 'cli',
    capabilities: {toolExecutors: {askQuestion: async (_question, options) => {
      if (!Array.isArray(options) || options.length === 0) throw new Error('Cline question has no available answer');
      return options[0];
    }}},
  });
  try {
    if (onEvent) core.subscribe(onEvent);
    const config = {
      cwd: workspace, workspaceRoot: workspace, providerId: provider, modelId: model,
      apiKey, baseUrl,
      systemPrompt: coreModule.getClineDefaultSystemPrompt({rootPath: workspace, platform: process.platform}),
      enableTools: true, enableSpawnAgent: true, enableAgentTeams: true, yolo: true,
      hookErrorMode: 'throw', hooks: native.hooks,
    };
    if (maxIterations !== undefined) config.maxIterations = maxIterations;
    const session = await core.start({config});
    const result = await core.send({sessionId: session.sessionId, prompt});
    // Child failures may be returned to the parent as tool results; settlement
    // must independently check the shared state even when the parent succeeds.
    native.assertSettled();
    if (!result || typeof result.finishReason !== 'string') throw new Error('Cline returned no terminal result');
    return result;
  } finally {
    await core.dispose();
  }
}
