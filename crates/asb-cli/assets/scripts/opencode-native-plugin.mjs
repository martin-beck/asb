// OpenCode owns tool semantics; this adapter only marks and replays tool calls.
import { brokerRequest } from './native-tool-broker.mjs';
import { snapshotBoundary } from './native-snapshot-boundary.mjs';
import { normalizeRequest } from './native-tool-session.mjs';

export function createOpenCodeNativePlugin({ mode, socket, marker, definitions = {} }) {
  if (!['record', 'replay'].includes(mode) || !socket) throw new TypeError('OpenCode native tool configuration is invalid');
  const active = new Map();
  const key = input => JSON.stringify([input.sessionID, input.callID]);
  const request = (name, args) => normalizeRequest({tool:name, input:args});
  // Delegation remains native so child sessions can make their own recorded calls.
  const delegated = name => name === 'task';
  if (mode === 'record') return {
    'tool.execute.before': async (input, output) => {
      if (delegated(input.tool)) return;
      const id = key(input);
      if (active.has(id)) throw new Error('OpenCode tool call is already active');
      const {token} = await brokerRequest(socket, {op:'start', request:request(input.tool, output.args)});
      active.set(id, token);
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
      } catch (error) { await brokerRequest(socket, {op:'invalidate',token}).catch(() => {}); active.delete(id); throw error; }
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
