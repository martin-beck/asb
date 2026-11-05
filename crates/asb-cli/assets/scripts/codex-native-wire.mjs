// Codex generates fresh UUIDs for locally created Responses input items on
// every launch. Keep their identity relationships within each request while
// leaving provider IDs, tool arguments and user content request-significant.
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
  const ids = body.input.filter(item => item?.type === 'function_call_output').map(item => item.call_id);
  if ((conversation !== null && conversation.length > 512) || ids.length > 16384 ||
      ids.some(id => typeof id !== 'string' || !id || id.length > 512)) return undefined;
  return {schema_version: 1, conversation, output_call_ids: ids};
}
