// Claude's WebFetch summarizes the fetched body in an ordinary recorded model
// request. Extract only the exact body bound to the native prompt and byte
// count; unknown template revisions and ambiguous separators remain errors.
const PREFIX = '\nWeb page content:\n---\n';
const SUMMARY = 'Provide a concise response based only on the content above.';

export function extractWebFetchPage(message, prompt, expectedBytes) {
  if (typeof message !== 'string' || typeof prompt !== 'string' || !prompt ||
      !Number.isSafeInteger(expectedBytes) || expectedBytes < 0)
    throw new TypeError('Claude WebFetch recorded page input is invalid');
  if (!message.startsWith(PREFIX)) throw new Error('Claude WebFetch summary template is unsupported');
  const delimiter = `\n---\n\n${prompt}\n\n${SUMMARY}`;
  const end = message.indexOf(delimiter, PREFIX.length);
  if (end < 0 || message.indexOf(delimiter, end + 1) >= 0)
    throw new Error('Claude WebFetch recorded page boundary is missing or ambiguous');
  const page = message.slice(PREFIX.length, end);
  if (Buffer.byteLength(page, 'utf8') !== expectedBytes)
    throw new Error('Claude WebFetch recorded page byte count differs');
  return page;
}

function summaryText(entry) {
  let body = entry?.request?.body;
  if (typeof body === 'string') {
    try { body = JSON.parse(body); } catch { return null; }
  }
  if (!body || typeof body !== 'object' || !Array.isArray(body.messages) || body.messages.length !== 1 ||
      (body.tools !== undefined && (!Array.isArray(body.tools) || body.tools.length !== 0))) return null;
  const message = body.messages[0];
  if (message?.role !== 'user') return null;
  if (typeof message.content === 'string') return message.content;
  if (!Array.isArray(message.content) || message.content.length !== 1 ||
      message.content[0]?.type !== 'text' || typeof message.content[0].text !== 'string') return null;
  return message.content[0].text;
}

// Inputs have already passed their respective cassette/sidecar validation.
// Return private in-memory replay data; callers own lifecycle and disclosure.
export function planWebFetchReplay(exchanges, tapeEntries) {
  if (!Array.isArray(exchanges) || !Array.isArray(tapeEntries)) throw new TypeError('Claude WebFetch replay evidence is invalid');
  const summaries = tapeEntries.map(summaryText).filter(text => text?.startsWith(PREFIX));
  const consumed = new Set(), plan = [], ordinals = new Set();
  for (const entry of [...exchanges].sort((a, b) => a.ordinal - b.ordinal)) {
    if (entry?.request?.tool !== 'WebFetch') continue;
    const ordinal = entry.ordinal, input = entry.request.input, value = entry.outcome?.value;
    if (!Number.isSafeInteger(ordinal) || ordinal < 0 || ordinals.has(ordinal) ||
        typeof input?.url !== 'string' || typeof input.prompt !== 'string' || !input.prompt ||
        entry.outcome?.ok !== true || value?.kind !== 'success' || typeof value.json !== 'string')
      throw new Error('Claude WebFetch native exchange is invalid or unsuccessful');
    ordinals.add(ordinal);
    let result;
    try { result = JSON.parse(value.json); } catch { throw new Error('Claude WebFetch native result is malformed'); }
    let url;
    try { url = new URL(input.url); } catch { throw new Error('Claude WebFetch recorded URL is invalid'); }
    if (!['http:', 'https:'].includes(url.protocol) || result?.url !== input.url ||
        !Number.isSafeInteger(result.bytes) || result.bytes < 0 || !Number.isSafeInteger(result.code))
      throw new Error('Claude WebFetch native result does not match the request');
    if ([301, 302, 303, 307, 308].includes(result.code)) {
      plan.push({ordinal, url: input.url, status: result.code, redirect: true});
      continue;
    }
    if (result.code !== 200) throw new Error('Claude WebFetch recorded status is unsupported');
    const matches = [];
    for (let index = 0; index < summaries.length; index++) {
      if (consumed.has(index)) continue;
      try { matches.push({index, page: extractWebFetchPage(summaries[index], input.prompt, result.bytes)}); }
      catch { /* A different recorded model call is not this fetch. */ }
    }
    if (matches.length !== 1) throw new Error('Claude WebFetch page evidence is missing or ambiguous');
    consumed.add(matches[0].index);
    plan.push({ordinal, url: input.url, status: 200, page: matches[0].page});
  }
  return plan;
}
