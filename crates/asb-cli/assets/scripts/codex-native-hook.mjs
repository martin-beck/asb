// Codex invokes shell lifecycle hooks as separate processes. Keep the adapter's
// native call identity private while the shared broker owns exchange ordering.
import { createHash } from 'node:crypto';
import { open, readFile, readdir, unlink } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
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
    if (typeof event.tool_input.command !== 'string') throw new TypeError('Codex shell command is missing');
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
    const completion = Buffer.from(JSON.stringify({...event, hook_event_name:'PostToolUse'})).toString('base64');
    const command = `(\n${event.tool_input.command}\n)\n__asb_status=$?\n${quote(process.execPath)} ${quote(fileURLToPath(import.meta.url))} finish ${quote(completion)} >/dev/null 2>&1\nexit "$__asb_status"`;
    return {hookSpecificOutput:{hookEventName:'PreToolUse',permissionDecision:'allow',updatedInput:{...event.tool_input,command}}};
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

// The caller invokes this only after the owned agent process tree has stopped.
// Any surviving state represents a background command terminated at teardown.
export async function settlePending({directory, socket, marker, mode, broker}) {
  if (mode === 'replay') {
    // Once the owned agent tree has stopped it cannot observe the remaining
    // delay. Release outstanding background effects without extending teardown;
    // the shared replay session still applies them in recorded order.
    await Promise.all([...broker.pending.keys()].map(token => brokerRequest(socket, {op:'release', token}).catch(error => {
      if (broker.pending.has(token)) throw error;
    })));
    while (!broker.isIdle()) await new Promise(resolve => setTimeout(resolve, 25));
    return;
  }
  for (const name of (await readdir(directory)).filter(name => /^[a-f0-9]{64}\.json$/.test(name)).sort()) {
    const file = join(directory, name);
    const data = JSON.parse(await readFile(file, 'utf8'));
    if (!Number.isSafeInteger(data.token) || data.token < 0 || !Array.isArray(data.ids) || data.ids.length !== 2 || data.ids.some(id => typeof id !== 'string' || !id))
      throw new Error('Codex pending native hook state is invalid');
    const sequence = await snapshotBoundary(marker, 'record', 'end', data.token, 'codex');
    await brokerRequest(socket, {op:'finish', token:data.token,
      outcome:{ok:true,value:{session_id:data.ids[0],tool_use_id:data.ids[1]}}, snapshot_sequence:sequence});
    await unlink(file);
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
  const result = typeof outcome?.value === 'string' ? {output:outcome.value,running_wait_ms:null} : outcome?.value;
  if (!outcome?.ok || typeof result?.output !== 'string') throw new Error('Codex native replay result is unavailable');
  // Apply the recorded snapshot boundary before returning the recorded tool
  // output, then let Codex construct its own normal result envelope.
  const running = result.running_wait_ms !== null;
  if (running) {
    if (!Number.isSafeInteger(result.running_wait_ms) || result.running_wait_ms < 0) throw new Error('Codex running tool timing is invalid');
    const helper = spawn(process.execPath, [fileURLToPath(import.meta.url), 'release', socket, String(token), String(result.running_wait_ms)], {detached:true,stdio:'ignore'});
    helper.unref();
  } else {
    const {value} = await brokerRequest(socket, {op:'release', token});
    if (JSON.stringify(value) !== JSON.stringify(outcome.value)) throw new Error('Codex native replay result changed after reservation');
  }
  const argument = Buffer.from(JSON.stringify(result), 'utf8').toString('base64');
  const command = `${quote(process.execPath)} ${quote(fileURLToPath(import.meta.url))} emit ${quote(argument)} ${quote(String(result.running_wait_ms ?? 0))}`;
  return {hookSpecificOutput: {hookEventName:'PreToolUse', permissionDecision:'allow', updatedInput:{...event.tool_input, command}}};
}

export async function emitRecorded(value, waitMs = 0, followups = []) {
  const match = value.match(/^Chunk ID: [^\r\n]+\r?\nWall time: (?:\d+(?:\.\d+)?|<asb-wall-time>) seconds\r?\nProcess exited with code (-?\d+)\r?\n(?:Original token count: (?:\d+|<asb-token-count>)\r?\n)?Output:\r?\n([\s\S]*)$/);
  if (!match) {
    const running = value.match(/^Chunk ID: [^\r\n]+\r?\nWall time: (?:\d+(?:\.\d+)?|<asb-wall-time>) seconds\r?\nProcess running with session ID [^\r\n]+\r?\n(?:Original token count: (?:\d+|<asb-token-count>)\r?\n)?Output:\r?\n([\s\S]*)$/);
    if (!running || !Number.isSafeInteger(waitMs) || waitMs <= 0) throw new Error('Codex native replay result has an unsupported shell envelope');
    process.stdout.write(running[1]);
    await new Promise(resolve => setTimeout(resolve, waitMs));
    for (const followup of followups) await emitRecorded(followup.output, followup.running_wait_ms ?? 0);
    return;
  }
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
  if (mode === 'record') {
    const response = await recordHook(event, options);
    if (response) process.stdout.write(`${JSON.stringify(response)}\n`);
  }
  else if (mode === 'replay') {
    const response = await replayHook(event, options);
    process.stdout.write(`${JSON.stringify(response)}\n`);
  } else throw new Error('Codex native hook mode is invalid');
}

if (process.argv[1] === fileURLToPath(import.meta.url) && process.argv[2] === 'emit') {
  const value = Buffer.from(process.argv[3] ?? '', 'base64').toString('utf8');
  let result; try { result=JSON.parse(value); } catch { result={output:value}; }
  if (typeof result === 'string') result={output:result};
  emitRecorded(result.output, Number(process.argv[4] ?? 0), result.followups ?? []).catch(error => {console.error(error.message);process.exitCode=1;});
} else if (process.argv[1] === fileURLToPath(import.meta.url) && process.argv[2] === 'hook') {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
} else if (process.argv[1] === fileURLToPath(import.meta.url) && process.argv[2] === 'finish') {
  const event = JSON.parse(Buffer.from(process.argv[3] ?? '', 'base64').toString('utf8'));
  recordHook(event, {directory:process.env.ASB_CODEX_NATIVE_DIRECTORY,socket:process.env.ASB_CODEX_NATIVE_SOCKET,marker:process.env.ASB_CODEX_NATIVE_MARKER})
    .catch(error => {console.error(error.message);process.exitCode=1;});
} else if (process.argv[1] === fileURLToPath(import.meta.url) && process.argv[2] === 'release') {
  const [, , , socket, token, wait] = process.argv;
  setTimeout(() => brokerRequest(socket, {op:'release',token:Number(token)}).catch(() => {process.exitCode=1;}), Number(wait));
}
