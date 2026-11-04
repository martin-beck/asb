export function getClineDefaultSystemPrompt({rootPath}) { return `workspace:${rootPath}`; }
export class ClineCore {
  static async create() {
    let listener;
    return {
      subscribe(callback) { listener = callback; },
      async start({config}) {
        if (config.providerId !== 'fixture-provider' || config.modelId !== 'fixture-model' ||
            config.hookErrorMode !== 'throw' || !config.enableSpawnAgent || !config.enableAgentTeams)
          throw new Error('incorrect Cline launcher configuration');
        return {sessionId: 'fixture-session'};
      },
      async send({prompt}) {
        if (prompt !== 'fixture prompt') throw new Error('incorrect Cline prompt');
        listener({type: 'agent_event', payload: {event: {type: 'fixture-event'}}});
        return {finishReason: 'completed', text: 'fixture answer', iterations: 2, usage: {inputTokens: 3}};
      },
      async getAccumulatedUsage() { return {aggregateUsage: {inputTokens: 7, outputTokens: 2}}; },
      async dispose() {},
    };
  }
}
