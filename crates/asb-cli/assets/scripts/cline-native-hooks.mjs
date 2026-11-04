// Cline's official core calls these hooks for root and delegated agents.
// Transport owns native exchange ordering and workspace-effect boundaries.
function canonical(value) {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return JSON.stringify(value);
  if (typeof value === 'number' && Number.isFinite(value)) return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value && typeof value === 'object' && Object.getPrototypeOf(value) === Object.prototype)
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  throw new TypeError('Cline native tool value is not representable as JSON');
}

export function createClineNativeHooks(mode, transport) {
  if (!['record', 'replay'].includes(mode) || !transport) throw new TypeError('Invalid Cline native replay configuration');
  const active = new Map();
  let failure;
  const key = ({snapshot, toolCall}) => {
    if (typeof snapshot?.agentId !== 'string' || typeof toolCall?.toolCallId !== 'string')
      throw new TypeError('Cline native tool identity is missing');
    return `${snapshot.agentId}\0${toolCall.toolCallId}`;
  };
  const fail = error => { failure ??= error; throw error; };
  const delegation = name => name === 'spawn_agent' || name.startsWith('team_');
  const hooks = {
    async beforeTool(context) {
      try {
        const id = key(context);
        if (active.has(id)) throw new Error('Cline native tool identity is already active');
        const name = context.tool?.name;
        if (typeof name !== 'string' || !name) throw new TypeError('Cline native tool name is missing');
        const request = JSON.parse(canonical({tool: name, input: context.input}));
        const delegates = delegation(name);
        if (mode === 'record') {
          const token = await transport.start(request);
          active.set(id, {token, delegates});
          try { await transport.begin(request, token); }
          catch (error) {
            await transport.invalidate(token).catch(() => {});
            throw error;
          }
        } else {
          const outcome = transport.invoke(request);
          // Observe rejection even while a delegated agent is executing.
          outcome.catch(() => {});
          active.set(id, {outcome, delegates});
          if (!delegates) {
            await outcome;
            return {skip: true, reason: 'Replaying recorded native tool result'};
          }
        }
      } catch (error) { fail(error); }
    },
    async afterTool(context) {
      try {
        const id = key(context);
        const entry = active.get(id);
        if (!entry) throw new Error('Cline native tool completion has no invocation');
        if (mode === 'record') {
          const result = JSON.parse(canonical(context.result));
          await transport.end(entry.token);
          await transport.finish(entry.token, result);
        } else {
          const expected = await entry.outcome;
          if (entry.delegates) {
            if (canonical(expected) !== canonical(context.result)) throw new Error('Cline delegated tool result differs from recording');
          } else {
            active.delete(id);
            return {result: expected};
          }
        }
        active.delete(id);
      } catch (error) { fail(error); }
    },
  };
  return {
    hooks,
    assertSettled() {
      if (failure) throw new Error('Cline native tool capture or replay failed', {cause: failure});
      if (active.size) throw new Error('Cline native tool calls did not settle');
    },
  };
}
