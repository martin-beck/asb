// Codex invokes shell lifecycle hooks as separate processes. Keep the adapter's
// native call identity private while the shared broker owns exchange ordering.
import { createHash } from 'node:crypto';
import { open, readFile, unlink } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
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

const quote = value => `'${value.replaceAll("'", "'\\''")}'`;

export async function replayHook(event, {socket}) {
  identity(event);
  if (!socket || event.hook_event_name !== 'PreToolUse' || event.tool_name !== 'Bash' ||
      !event.tool_input || typeof event.tool_input !== 'object' || Array.isArray(event.tool_input))
    throw new TypeError('Codex native replay hook input is invalid');
  const request = normalizeRequest({tool: event.tool_name, input: event.tool_input});
  const {token} = await brokerRequest(socket, {op:'reserve', request});
  const {outcome} = await brokerRequest(socket, {op:'inspect', token});
  if (!outcome?.ok || typeof outcome.value !== 'string') throw new Error('Codex native replay result is unavailable');
  // Apply the recorded snapshot boundary before returning the recorded tool
  // output, then let Codex construct its own normal result envelope.
  const {value} = await brokerRequest(socket, {op:'release', token});
  if (value !== outcome.value) throw new Error('Codex native replay result changed after reservation');
  const argument = Buffer.from(value, 'utf8').toString('base64');
  const command = `${quote(process.execPath)} ${quote(fileURLToPath(import.meta.url))} emit ${quote(argument)}`;
  return {hookSpecificOutput: {hookEventName:'PreToolUse', permissionDecision:'allow', updatedInput:{...event.tool_input, command}}};
}

export function emitRecorded(value) {
  const match = value.match(/^Chunk ID: [^\r\n]+\r?\nWall time: \d+(?:\.\d+)? seconds\r?\nProcess exited with code (-?\d+)\r?\n(?:Original token count: \d+\r?\n)?Output:\r?\n([\s\S]*)$/);
  if (!match) throw new Error('Codex native replay result has an unsupported shell envelope');
  const code = Number(match[1]);
  if (!Number.isInteger(code) || code < 0 || code > 255) throw new Error('Codex native replay exit status is invalid');
  process.stdout.write(match[2]);
  process.exitCode = code;
}

export async function main() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  const event = JSON.parse(Buffer.concat(chunks).toString('utf8'));
  const options = {directory:process.env.ASB_CODEX_NATIVE_DIRECTORY, socket:process.env.ASB_CODEX_NATIVE_SOCKET, marker:process.env.ASB_CODEX_NATIVE_MARKER};
  const mode = process.env.ASB_CODEX_NATIVE_MODE;
  if (mode === 'record') await recordHook(event, options);
  else if (mode === 'replay') {
    const response = await replayHook(event, options);
    process.stdout.write(`${JSON.stringify(response)}\n`);
  } else throw new Error('Codex native hook mode is invalid');
}

if (process.argv[1] === fileURLToPath(import.meta.url) && process.argv[2] === 'emit') {
  const value = Buffer.from(process.argv[3] ?? '', 'base64').toString('utf8');
  emitRecorded(value);
} else if (process.argv[1] === fileURLToPath(import.meta.url) && process.argv[2] === 'hook') {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
