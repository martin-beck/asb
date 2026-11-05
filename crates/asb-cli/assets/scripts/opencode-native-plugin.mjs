// OpenCode owns tool semantics; this adapter only marks and replays tool calls.
import { brokerRequest } from './native-tool-broker.mjs';
import { snapshotBoundary } from './native-snapshot-boundary.mjs';
import { normalizeRequest } from './native-tool-session.mjs';
import { createHash } from 'node:crypto';
import { writeFileSync, unlinkSync } from 'node:fs';
import { readFile, readdir, unlink } from 'node:fs/promises';
import { join } from 'node:path';

export function createOpenCodeNativePlugin({ mode, socket, marker, directory, definitions = {} }) {
  if (!['record', 'replay'].includes(mode) || !socket) throw new TypeError('OpenCode native tool configuration is invalid');
  const active = new Map();
  const settlements = new Set();
  const key = input => JSON.stringify([input.sessionID, input.callID]);
  const statePath = id => directory && join(directory, createHash('sha256').update(id).digest('hex')+'.json');
  const clear = id => { const path=statePath(id); if (path) { try { unlinkSync(path); } catch (error) { if (error.code !== 'ENOENT') throw error; } } };
  const request = (name, args) => normalizeRequest({tool:name, input:args});
  // Delegation remains native so child sessions can make their own recorded calls.
  const delegated = name => name === 'task';
  if (mode === 'record') return {
    'tool.execute.before': async (input, output) => {
      if (delegated(input.tool)) return;
      await Promise.all([...settlements]);
      const id = key(input);
      if (active.has(id)) throw new Error('OpenCode tool call is already active');
      const {token} = await brokerRequest(socket, {op:'start', request:request(input.tool, output.args)});
      active.set(id, token);
      if (directory) writeFileSync(statePath(id), JSON.stringify({token}), {mode:0o600, flag:'wx'});
      try { await snapshotBoundary(marker, 'record', 'begin', token, 'opencode'); }
      catch (error) { active.delete(id); await brokerRequest(socket, {op:'invalidate', token}).catch(() => {}); throw error; }
    },
    'tool.execute.after': async (input, output) => {
      if (delegated(input.tool)) return;
      const id = key(input), token = active.get(id);
      if (token === undefined) throw new Error('OpenCode tool completion has no matching start');
      try {
        const sequence = await snapshotBoundary(marker, 'record', 'end', token, 'opencode');
        const message = {op:'finish',token,outcome:{ok:true,value:{title:output.title,output:output.output,metadata:output.metadata}}};
        if (sequence !== undefined) message.snapshot_sequence = sequence;
        await brokerRequest(socket, message);
        active.delete(id);
        clear(id);
      } catch (error) { await brokerRequest(socket, {op:'invalidate',token}).catch(() => {}); active.delete(id); throw error; }
    },
    // OpenCode only invokes execute.after on success. Failed calls are
    // published as tool-part updates instead, including their actual error.
    event: ({event}) => {
      const part = event?.type === 'message.part.updated' ? event.properties?.part : undefined;
      if (part?.type !== 'tool' || part.state?.status !== 'error' || delegated(part.tool)) return;
      const id = key({sessionID:part.sessionID, callID:part.callID}), token = active.get(id);
      if (token === undefined) return;
      active.delete(id);
      const reason=String(part.state.error ?? 'OpenCode tool failed');
      if (directory) writeFileSync(statePath(id), JSON.stringify({token,error:reason}), {mode:0o600});
      const settlement = (async () => { try {
        const sequence = await snapshotBoundary(marker, 'record', 'end', token, 'opencode');
        if (directory && sequence !== undefined) writeFileSync(statePath(id), JSON.stringify({token,error:reason,sequence}), {mode:0o600});
        const message = {op:'finish',token,outcome:{ok:false,name:'Error',message:reason}};
        if (sequence !== undefined) message.snapshot_sequence = sequence;
        await brokerRequest(socket, message);
        clear(id);
      } catch (error) { await brokerRequest(socket, {op:'invalidate',token}).catch(() => {}); throw error; } })();
      settlements.add(settlement);
      settlement.finally(() => settlements.delete(settlement)).catch(() => {});
      return settlement;
    },
  };
  const tool = {};
  for (const [name, definition] of Object.entries(definitions)) {
    if (delegated(name)) continue;
    if (!definition || typeof definition.description !== 'string' || !definition.schema || typeof definition.schema !== 'object' || Array.isArray(definition.schema)) throw new TypeError('OpenCode replay tool definition is invalid');
    const properties = definition.schema.properties ?? {};
    tool[name] = {
      description:definition.description,
      // JSON Schema entries keep OpenCode's plugin adapter from stripping
      // optional arguments; the definition hook restores the full schema.
      args:Object.fromEntries(Object.keys(properties).map(key => [key, true])),
      execute: async args => brokerRequest(socket, {op:'invoke', request:request(name, args)}).then(result => result.value),
    };
  }
  return {
    tool,
    'tool.definition': async (input, output) => {
      const definition = definitions[input.toolID];
      if (definition && !delegated(input.toolID)) {
        output.description = definition.description;
        output.jsonSchema = structuredClone(definition.schema);
      }
    },
  };
}

// Called by the owner after the entire agent process tree has stopped.
export async function settlePending({directory, socket, marker, mode, broker}) {
  if (mode !== 'record') return;
  for (const name of (await readdir(directory)).filter(name => /^[a-f0-9]{64}\.json$/.test(name)).sort()) {
    const path=join(directory,name), state=JSON.parse(await readFile(path,'utf8'));
    if (!broker.pending.has(state.token)) { await unlink(path); continue; }
    if (!Number.isSafeInteger(state.token) || typeof state.error !== 'string') throw new Error('OpenCode tool did not report its outcome before shutdown');
    const sequence=state.sequence ?? await snapshotBoundary(marker,'record','end',state.token,'opencode');
    const message={op:'finish',token:state.token,outcome:{ok:false,name:'Error',message:state.error}};
    if (sequence !== undefined) message.snapshot_sequence=sequence;
    await brokerRequest(socket,message);
    await unlink(path);
  }
}
