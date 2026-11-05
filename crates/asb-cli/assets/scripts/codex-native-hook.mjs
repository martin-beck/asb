// Codex invokes shell lifecycle hooks as separate processes. Keep the adapter's
// native call identity private while the shared broker owns exchange ordering.
import { createHash } from 'node:crypto';
import { open, readFile, unlink } from 'node:fs/promises';
import { join } from 'node:path';
import { brokerRequest } from './native-tool-broker.mjs';
import { normalizeRequest } from './native-tool-session.mjs';
import { snapshotBoundary } from './native-snapshot-boundary.mjs';

function identity(event) {
  if (typeof event?.session_id !== 'string' || !event.session_id ||
      typeof event?.tool_use_id !== 'string' || !event.tool_use_id)
    throw new TypeError('Codex native hook session or call identity is missing');
  return [event.session_id, event.tool_use_id];
}

export async function recordHook(event, {directory, socket, marker}) {
  const ids = identity(event);
  if (!directory || !socket || !marker || typeof event.tool_name !== 'string' || !event.tool_name ||
      !event.tool_input || typeof event.tool_input !== 'object' || Array.isArray(event.tool_input))
    throw new TypeError('Codex native hook input is invalid');
  const state = join(directory, createHash('sha256').update(JSON.stringify(ids)).digest('hex') + '.json');
  const request = normalizeRequest({tool: event.tool_name, input: event.tool_input});
  if (event.hook_event_name === 'PreToolUse') {
    const file = await open(state, 'wx', 0o600);
    let token;
    try {
      ({token} = await brokerRequest(socket, {op: 'start', request}));
      await file.writeFile(JSON.stringify({token, request, ids}));
      await file.close();
      await snapshotBoundary(marker, 'record', 'begin', token, 'codex');
    } catch (error) {
      await file.close().catch(() => {});
      if (token !== undefined) await brokerRequest(socket, {op: 'invalidate', token}).catch(() => {});
      throw error;
    }
    return;
  }
  if (event.hook_event_name !== 'PostToolUse') throw new TypeError('Codex native hook event is unsupported');
  const data = JSON.parse(await readFile(state, 'utf8'));
  if (!Number.isSafeInteger(data.token) || data.token < 0 ||
      JSON.stringify(data.request) !== JSON.stringify(request) || JSON.stringify(data.ids) !== JSON.stringify(ids))
    throw new Error('Codex native hook completion does not match its start');
  try {
    // The hook response omits the exit code. Bind the complete result from the
    // private, settled provider capture before publishing the sidecar.
    const sequence = await snapshotBoundary(marker, 'record', 'end', data.token, 'codex');
    await brokerRequest(socket, {op: 'finish', token: data.token,
      outcome: {ok: true, value: {session_id: ids[0], tool_use_id: ids[1]}}, snapshot_sequence: sequence});
    await unlink(state);
  } catch (error) {
    await brokerRequest(socket, {op: 'invalidate', token: data.token}).catch(() => {});
    throw error;
  }
}
