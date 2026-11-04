import {pathToFileURL} from 'node:url';
import {createClineNativeTransport} from './cline-native-transport.mjs';
import {runClineNativeSession} from './cline-native-session.mjs';

// Arguments are provided by the Cline adapter; credentials remain in the
// private agent environment and are never added to the command line.
export async function main(args = process.argv.slice(1), env = process.env) {
  const [provider, model, prompt, limit] = args;
  if (!env.ASB_CLINE_CORE_MODULE || !env.ASB_CLINE_NATIVE_SOCKET || !env.ASB_CLINE_MARKER || !env.ASB_CLINE_NATIVE_MODE)
    throw new Error('Cline native session was not prepared');
  const coreModule = await import(pathToFileURL(env.ASB_CLINE_CORE_MODULE).href);
  const transport = createClineNativeTransport(env.ASB_CLINE_NATIVE_SOCKET, env.ASB_CLINE_MARKER);
  const result = await runClineNativeSession({
    coreModule, transport, mode: env.ASB_CLINE_NATIVE_MODE,
    workspace: process.cwd(), provider, model, prompt,
    baseUrl: env.OPENAI_BASE_URL, apiKey: env.CLINE_API_KEY,
    maxIterations: limit ? Number(limit) : undefined,
    onEvent(event) {
      if (event?.type === 'agent_event' && event.payload?.event)
        process.stdout.write(`${JSON.stringify({type: 'agent_event', event: event.payload.event})}\n`);
      else if (event?.type === 'hook' && event.payload)
        process.stdout.write(`${JSON.stringify({type: 'hook_event', ...event.payload})}\n`);
    },
  });
  process.stdout.write(`${JSON.stringify({
    type: 'run_result', finishReason: result.finishReason, text: result.text,
    iterations: result.iterations, usage: result.usage, aggregateUsage: result.aggregateUsage,
  })}\n`);
  if (result.finishReason !== 'completed') process.exitCode = 1;
}
