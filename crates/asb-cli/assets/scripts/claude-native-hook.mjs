// Claude's command hooks run in separate processes. This adapter-owned bridge
// correlates a native call's start and completion with the shared broker.
import { createHash } from 'node:crypto';
import { open, readFile, unlink, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { brokerRequest } from './native-tool-broker.mjs';
import { normalizeRequest } from './native-tool-session.mjs';
import { snapshotBoundary } from './native-snapshot-boundary.mjs';

function statePath(directory, event) {
  if (!directory || typeof event?.session_id !== 'string' || !event.session_id ||
      typeof event?.tool_use_id !== 'string' || !event.tool_use_id)
    throw new TypeError('Claude native hook identity is missing');
  if (event.agent_id !== undefined && (typeof event.agent_id !== 'string' || !event.agent_id))
    throw new TypeError('Claude native hook agent identity is invalid');
  const id = createHash('sha256').update(JSON.stringify([event.session_id, event.agent_id ?? null, event.tool_use_id])).digest('hex');
  return join(directory, `${id}.json`);
}

export async function recordHook(event, { directory, socket, marker }) {
  const state = statePath(directory, event);
  if (!socket || !marker || typeof event.tool_name !== 'string' || !event.tool_name ||
      !event.tool_input || typeof event.tool_input !== 'object' || Array.isArray(event.tool_input))
    throw new TypeError('Claude native hook input is incomplete');
  const request = normalizeRequest({tool: event.tool_name, input: event.tool_input});
  if (event.hook_event_name === 'PreToolUse') {
    // Claim the identity before issuing any broker mutation. A repeated or
    // concurrent identity must not replace an in-flight exchange.
    const file = await open(state, 'wx', 0o600);
    let token;
    try {
      ({token} = await brokerRequest(socket, {op: 'start', request}));
      await file.writeFile(JSON.stringify({token, request}));
      await file.close();
      await snapshotBoundary(marker, 'record', 'begin', token, 'claude-code');
    } catch (error) {
      await file.close().catch(() => {});
      if (token !== undefined) await brokerRequest(socket, {op: 'invalidate', token}).catch(() => {});
      throw error;
    }
    return;
  }
  if (!['PostToolUse', 'PostToolUseFailure'].includes(event.hook_event_name))
    throw new TypeError('Claude native hook event is unsupported');
  const data = JSON.parse(await readFile(state, 'utf8'));
  if (!Number.isSafeInteger(data.token) || data.token < 0 || JSON.stringify(data.request) !== JSON.stringify(request))
    throw new Error('Claude native hook completion does not match its start');
  try {
    // Preserve the serialized order that Claude uses in provider-visible output.
    const result = event.hook_event_name === 'PostToolUse'
      ? {kind: 'success', json: JSON.stringify(event.tool_response)}
      : {kind: 'failure', json: JSON.stringify({error: event.error, is_interrupt: event.is_interrupt})};
    if (typeof result.json !== 'string') throw new TypeError('Claude native hook result is missing');
    await snapshotBoundary(marker, 'record', 'end', data.token, 'claude-code');
    await brokerRequest(socket, {op: 'finish', token: data.token, outcome: {ok: true, value: result}});
    await unlink(state);
  } catch (error) {
    await brokerRequest(socket, {op: 'invalidate', token: data.token}).catch(() => {});
    throw error;
  }
}

// Experimental native replay probe; WebFetch requires a local replay proxy. Production must handle all native
// tools and failure shapes before this can be selected by the adapter.
export async function replayHook(event, { directory, socket, webProxy }) {
  const state = statePath(directory, event);
  if (!socket || !['Bash', 'Read', 'Edit', 'WebSearch', 'WebFetch'].includes(event.tool_name) || !event.tool_input ||
      typeof event.tool_input !== 'object' || Array.isArray(event.tool_input))
    throw new TypeError('Claude native replay probe does not support this native tool');
  if (event.tool_name === 'WebFetch' && !webProxy)
    throw new Error('Claude WebFetch replay requires a local recorded-content proxy');
  if (event.hook_event_name === 'PreToolUse') {
    const file = await open(state, 'wx', 0o600);
    try {
      const request = normalizeRequest({tool: event.tool_name, input: event.tool_input});
      const {token} = await brokerRequest(socket, {op: 'reserve', request});
      const {outcome} = await brokerRequest(socket, {op: 'inspect', token});
      if (!outcome?.ok || !['success', 'failure'].includes(outcome.value?.kind) || typeof outcome.value.json !== 'string')
        throw new Error('Claude native replay recorded outcome is invalid');
      const expected = outcome.value;
      let command = ':';
      if (event.tool_name === 'Bash' && expected.kind === 'failure') {
        const failure = JSON.parse(expected.json);
        const match = typeof failure.error === 'string' && failure.error.match(/^Exit code ([1-9][0-9]{0,2})(?:\n([\s\S]*))?$/);
        if (!match || Number(match[1]) > 255 || failure.is_interrupt !== false)
          throw new Error('Claude native replay probe cannot reproduce this Bash failure');
        const text = (match[2] ?? '').replaceAll("'", "'\\''");
        command = `printf '%s' '${text}'; exit ${match[1]}`;
      }
      // Claude tracks paths read and edited by its own tools. Execute these
      // deterministic file operations in the isolated replay workspace, then
      // reconcile the recorded boundary before publishing the recorded result.
      const substitute = normalizeRequest(event.tool_name === 'Bash' ? {...event.tool_input, command} : event.tool_input);
      await file.writeFile(JSON.stringify({token, substitute, expected}));
      await file.close();
      return {hookSpecificOutput: {hookEventName: 'PreToolUse', permissionDecision: 'allow', updatedInput: substitute}};
    } catch (error) {
      await file.close().catch(() => {});
      throw error;
    }
  }
  if (!['PostToolUse', 'PostToolUseFailure'].includes(event.hook_event_name))
    throw new Error('Claude native replay probe expected a tool completion');
  const data = JSON.parse(await readFile(state, 'utf8'));
  if (!Number.isSafeInteger(data.token) || data.token < 0 ||
      JSON.stringify(data.substitute) !== JSON.stringify(normalizeRequest(event.tool_input)))
    throw new Error('Claude native replay completion does not match its start');
  const {value} = await brokerRequest(socket, {op: 'release', token: data.token});
  if (JSON.stringify(value) !== JSON.stringify(data.expected))
    throw new Error('Claude native replay outcome changed after reservation');
  if (value.kind === 'failure') {
    const actual = JSON.stringify({error: event.error, is_interrupt: event.is_interrupt});
    if (event.hook_event_name !== 'PostToolUseFailure' || actual !== value.json)
      throw new Error('Claude native replay failure differs from recording');
    await unlink(state);
    return {};
  }
  if (event.hook_event_name !== 'PostToolUse') throw new Error('Claude native replay expected a successful tool');
  const result = JSON.parse(value.json);
  await unlink(state);
  return {hookSpecificOutput: {hookEventName: 'PostToolUse', updatedToolOutput: result}};
}

export async function main() {
  const directory = process.env.ASB_CLAUDE_NATIVE_DIRECTORY;
  try {
    const chunks = [];
    let size = 0;
    for await (const chunk of process.stdin) {
      size += chunk.length;
      if (size > 16 * 1024 * 1024) throw new Error('Claude native hook input exceeds limit');
      chunks.push(chunk);
    }
    const event = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    const options = {directory, socket: process.env.ASB_CLAUDE_NATIVE_SOCKET, marker: process.env.ASB_CLAUDE_NATIVE_MARKER, webProxy: process.env.ASB_CLAUDE_NATIVE_WEB_PROXY};
    const mode = process.env.ASB_CLAUDE_NATIVE_MODE ?? 'record';
    if (!['record', 'replay'].includes(mode)) throw new Error('Claude native hook mode is invalid');
    const result = mode === 'replay' ? await replayHook(event, options) : await recordHook(event, options);
    process.stdout.write(`${JSON.stringify(result ?? {})}\n`);
  } catch (error) {
    if (directory) await writeFile(join(directory, 'hook-failed'), 'Claude native hook failed\n', {flag: 'a', mode: 0o600}).catch(() => {});
    throw error;
  }
}
