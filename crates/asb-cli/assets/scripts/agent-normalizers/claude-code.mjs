// Claude Code creates a per-process telemetry identifier in this exact
// provider metadata field.  Nothing else in the request is changed.
export function normalizeRequest(request) {
  const body = request?.body;
  if (body?.metadata && typeof body.metadata === "object" && !Array.isArray(body.metadata)
      && typeof body.metadata.user_id === "string") {
    return { ...body, metadata: { ...body.metadata, user_id: "<asb-user-id>" } };
  }
  return body;
}

// Parallel tool results can arrive in completion order. Compare complete,
// unambiguous result batches in the order of their preceding tool calls.
export function normalizeComparison(request) {
  const body = request?.body;
  if (!Array.isArray(body?.messages)) return body;
  const messages = body.messages.map((message, index) => {
    const previous = body.messages[index - 1];
    if (message?.role !== 'user' || previous?.role !== 'assistant' ||
        !Array.isArray(message.content) || !Array.isArray(previous.content) ||
        message.content.length < 2 || message.content.some((item) => item?.type !== 'tool_result')) return message;
    const calls = previous.content.filter((item) => item?.type === 'tool_use');
    const ids = calls.map((item) => item.id);
    const resultIds = message.content.map((item) => item.tool_use_id);
    if (calls.length !== message.content.length || ids.some((id) => typeof id !== 'string' || !id) ||
        new Set(ids).size !== ids.length || new Set(resultIds).size !== resultIds.length ||
        resultIds.some((id) => !ids.includes(id))) return message;
    // A cache marker on the final block marks the whole batch boundary; it
    // follows that boundary when the batch is reordered. Other cache layouts
    // have different prefix semantics and must remain untouched.
    if (message.content.slice(0, -1).some((item) => Object.hasOwn(item, 'cache_control'))) return message;
    const order = new Map(ids.map((id, i) => [id, i]));
    const last = message.content.at(-1);
    const hasCache = Object.hasOwn(last, 'cache_control');
    const ordered = message.content.map((item) => {
      if (!hasCache || item !== last) return item;
      const {cache_control: _cache, ...rest} = item;
      return rest;
    }).sort((a, b) => order.get(a.tool_use_id) - order.get(b.tool_use_id));
    if (hasCache) ordered[ordered.length - 1] = {...ordered.at(-1), cache_control:last.cache_control};
    return {...message, content: ordered};
  });
  return {...body, messages};
}
