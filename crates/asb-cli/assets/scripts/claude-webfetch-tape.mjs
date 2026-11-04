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
