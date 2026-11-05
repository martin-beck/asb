// Codex generates fresh UUIDs for locally created Responses input items on
// every launch. Keep their identity relationships within each request while
// leaving provider IDs, tool arguments and user content request-significant.
const processAliases = new Map();
const replayProcesses = new Map();
let nextProcess = 1;
let processMappingEnabled = true;
export function configureReplay({interactions}) {
  processMappingEnabled = interactions.some(row => Array.isArray(row?.agent_record_metadata?.running_session_ids));
}
const runningId = output => typeof output === 'string' ? output.match(/^Chunk ID: [^\r\n]+\r?\nWall time: \d+(?:\.\d+)? seconds\r?\nProcess running with session ID (\d+)/)?.[1] : undefined;
function alias(id) {
  if (!processAliases.has(id)) processAliases.set(id, `<asb-codex-process-${nextProcess++}>`);
  return processAliases.get(id);
}

export function normalizeRequest({ url, body }) {
  if (typeof url !== 'string' || !/\/responses(?:\?|$)/.test(url) ||
      !body || typeof body !== 'object') return body;
  if (typeof body.prompt_cache_key === 'string' &&
      /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(body.prompt_cache_key)) {
    body.prompt_cache_key = '<asb-codex-session-cache>';
  }
  if (!Array.isArray(body.input)) return body;
  const ids = new Map();
  for (const item of body.input) {
    if (processMappingEnabled && item?.type === 'function_call_output') {
      const session = runningId(item.output);
      if (session) alias(session);
    }
    if (processMappingEnabled && item?.type === 'function_call' && item.name === 'write_stdin' && typeof item.arguments === 'string') {
      try {
        const args = JSON.parse(item.arguments), id = String(args.session_id);
        if (processAliases.has(id)) item.arguments = JSON.stringify({...args, session_id:processAliases.get(id)});
      } catch { /* Leave unsupported argument forms untouched. */ }
    }
    if (!item || typeof item !== 'object' || typeof item.id !== 'string') continue;
    const message = item.type === 'message' && (item.role === 'developer' || item.role === 'user');
    const toolOutput = item.type === 'function_call_output';
    const prefix = message ? 'msg' : toolOutput ? 'fco' : null;
    if (!prefix || !new RegExp(`^${prefix}_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`, 'i').test(item.id)) continue;
    if (!ids.has(item.id)) ids.set(item.id, `<asb-codex-item-${ids.size + 1}>`);
    item.id = ids.get(item.id);
  }
  return body;
}

// The normalized cassette key deliberately replaces generated call IDs.
// Retain their private, per-conversation relationship for the native adapter;
// these values are never added to the provider request or matching key.
export function recordMetadata({url, body}) {
  if (typeof url !== 'string' || !/\/responses(?:\?|$)/.test(url) ||
      !body || typeof body !== 'object' || !Array.isArray(body.input)) return undefined;
  const conversation = typeof body.prompt_cache_key === 'string' ? body.prompt_cache_key : null;
  const outputs = body.input.filter(item => item?.type === 'function_call_output');
  const ids = outputs.map(item => item.call_id);
  if (ids.some(id => typeof id !== 'string' || !id)) return undefined;
  const running_wait_ms = outputs.map(item => {
    if (typeof item.output !== 'string') return null;
    const match = item.output.match(/^Chunk ID: [^\r\n]+\r?\nWall time: (\d+(?:\.\d+)?) seconds\r?\nProcess running with session ID /);
    if (!match) return null;
    const duration = Number(match[1]);
    return Number.isFinite(duration) && duration >= 0 ? Math.ceil(duration * 1000) + 250 : null;
  });
  return {schema_version: 1, conversation, output_call_ids: ids, running_wait_ms,
    running_session_ids: outputs.map(item => runningId(item.output) ?? null)};
}

// Called only after a request has matched its recorded counterpart. Output
// positions are part of that exact request, so this binding does not infer
// relationships from process creation order.
export function observeReplayExchange({body, interaction}) {
  if (!processMappingEnabled) return;
  if (!body || !Array.isArray(body.input)) return;
  const recorded = interaction?.agent_record_metadata?.running_session_ids;
  if (!Array.isArray(recorded)) return;
  const outputs = body.input.filter(item => item?.type === 'function_call_output');
  if (recorded.length !== outputs.length) throw new Error('Codex replay process metadata does not match the request');
  for (let i=0; i<outputs.length; i++) {
    const live = runningId(outputs[i].output), prior = recorded[i];
    if (prior === null && live === undefined) continue;
    if (typeof prior !== 'string' || !live || (replayProcesses.has(prior) && replayProcesses.get(prior) !== live))
      throw new Error('Codex replay process identity differs from its recording');
    replayProcesses.set(prior,live);
  }
}

export function rewriteReplayResponse({body}) {
  if (!processMappingEnabled) return body;
  if (typeof body !== 'string' || !body.includes('data:')) return body;
  const lines = body.split(/(?<=\n)/), events = new Map(), replacements = new Map();
  for (let i=0; i<lines.length; i++) {
    const match = lines[i].match(/^(data:\s+)(\{.*\})(\r?\n)?$/);
    if (!match) continue;
    let event; try { event=JSON.parse(match[2]); } catch { continue; }
    events.set(i,{event,prefix:match[1],newline:match[3] ?? ''});
    const item=event.type === 'response.output_item.done' ? event.item : undefined;
    if (item?.type !== 'function_call' || item.name !== 'write_stdin' || typeof item.arguments !== 'string') continue;
    const args=JSON.parse(item.arguments), id=String(args.session_id), live=replayProcesses.get(id);
    if (!live) throw new Error('Codex replay has no matching running process session');
    replacements.set(item.id, JSON.stringify({...args,session_id:Number(live)}));
  }
  const emitted = new Set();
  for (const {event} of events.values()) {
    const value = replacements.get(event.item_id);
    if (value !== undefined && event.type === 'response.function_call_arguments.delta') {
      event.delta = emitted.has(event.item_id) ? '' : value; emitted.add(event.item_id);
    }
    if (value !== undefined && event.type === 'response.function_call_arguments.done') event.arguments=value;
    if (event.type === 'response.output_item.done' && replacements.has(event.item?.id)) event.item.arguments=replacements.get(event.item.id);
    if (event.type === 'response.completed' && Array.isArray(event.response?.output))
      for (const item of event.response.output) if (replacements.has(item?.id)) item.arguments=replacements.get(item.id);
  }
  for (const [index, item] of events) lines[index]=`${item.prefix}${JSON.stringify(item.event)}${item.newline}`;
  return lines.join('');
}
