// Codex generates fresh UUIDs for locally created Responses input items on
// every launch. Keep their identity relationships within each request while
// leaving provider IDs, tool arguments and user content request-significant.
export function normalizeRequest({ url, body }) {
  if (typeof url !== 'string' || !/\/responses(?:\?|$)/.test(url) ||
      !body || typeof body !== 'object' || !Array.isArray(body.input)) return body;
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
