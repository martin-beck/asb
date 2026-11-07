// Exploratory request comparison. Candidate functions see only protocol-owned
// tool-result text and associated tool metadata, never the whole request.
import { normalizeToolResults, normalizeToolResultsAsync } from "./tool-result-normalization.mjs";

// An adapter service failure is not a nondeterministic tool result. Matching
// it to another output would hide a broken replay and poison future searches.
function localToolServiceFailure(text) {
  return text.startsWith('ASB local tool service exited with status ')
    || text.startsWith('ASB local tool service failed to start:');
}

function results(body) {
  const found = [];
  normalizeToolResults(body, ({ text, tool, location }) => {
    if (found.length >= 4096) throw new RangeError('discovery result limit');
    found.push({ text, tool, location });
    return text;
  });
  return found;
}

function sameRequest(expected, actual) {
  return expected.method === actual.method && expected.url === actual.url
    && JSON.stringify(expected.body) === JSON.stringify(actual.body);
}

// Reconstruct executable code from bounded data, never from a stored script.
export function temporaryNormalizerSource(entries) {
  if (!Array.isArray(entries) || entries.length < 1 || entries.length > 64) return null;
  const keys = new Set();
  for (const entry of entries) {
    if (!Array.isArray(entry) || entry.length !== 2 || typeof entry[0] !== 'string' || typeof entry[1] !== 'string'
        || Buffer.byteLength(entry[0]) > 32768 || Buffer.byteLength(entry[1]) > 16384 || keys.has(entry[0])) return null;
    keys.add(entry[0]);
    let key;
    try { key = JSON.parse(entry[0]); } catch { return null; }
    if (!Array.isArray(key) || key.length !== 2 || !key[0] || typeof key[0] !== 'object'
        || Array.isArray(key[0]) || typeof key[0].name !== 'string' || typeof key[1] !== 'string') return null;
    if (localToolServiceFailure(key[1]) || localToolServiceFailure(entry[1])) return null;
  }
  const json = JSON.stringify(entries);
  const data = Array.from({length: json.length}, (_, index) => `\\u${json.charCodeAt(index).toString(16).padStart(4, '0')}`).join('');
  const source = `const rules = new Map(JSON.parse("${data}"));\nexport function normalizeToolResult({text, tool}) {\n  return rules.get(JSON.stringify([tool, text])) ?? text;\n}\n`;
  return Buffer.byteLength(source) <= 65536 ? source : null;
}

// A match here is evidence about this request only, not the rest of a replay.
// Candidates are supplied by the caller; never load code from a cassette.
export async function findNormalizer(expected, actual, candidates) {
  if (expected.method !== actual.method || expected.url !== actual.url) return null;
  if (!expected.body || !actual.body || typeof expected.body !== "object" || typeof actual.body !== "object") return null;
  let left, right;
  try { left = results(expected.body); right = results(actual.body); }
  catch (error) { if (error instanceof RangeError) return {kind:'limited'}; throw error; }
  if (left.length !== right.length) return null;
  if (left.some((entry, index) => entry.location !== right[index].location
      || JSON.stringify(entry.tool) !== JSON.stringify(right[index].tool))) return null;
  const differing = left.map((entry, index) => ({expected: entry, actual: right[index]}))
    .filter(({expected: a, actual: b}) => a.text !== b.text);
  if (!differing.length || differing.some(({expected}) => !expected.tool)) return null;
  if (differing.some(({expected, actual}) => localToolServiceFailure(expected.text) || localToolServiceFailure(actual.text))) return null;
  const locations = new Set(differing.map(({expected}) => expected.location));
  const mask = (body) => normalizeToolResults(body, ({text, location}) => locations.has(location) ? '<asb-discovery-result>' : text);
  if (!sameRequest({...expected, body: mask(expected.body)}, {...actual, body: mask(actual.body)})) return null;

  for (const candidate of candidates) {
    let changed = false;
    try {
      const transform = (body) => normalizeToolResultsAsync(body, async ({text, tool, location}) => {
        if (!locations.has(location)) return text;
        const value = await candidate.normalizeToolResult({text, tool});
        if (typeof value !== "string") throw new TypeError("normalizer returned non-string");
        changed ||= value !== text;
        return value;
      });
      const normalizedExpected = {...expected, body: await transform(expected.body)};
      const normalizedActual = {...actual, body: await transform(actual.body)};
      if (changed && sameRequest(normalizedExpected, normalizedActual)) {
        return { kind: "existing", id: candidate.id, source: candidate.source };
      }
    } catch {
      // An unavailable diagnostic is not evidence that replay matches.
    } finally {
      try { await candidate.close?.(); } catch { /* The mismatch remains authoritative. */ }
    }
  }

  // Exact observed pairs are deliberately narrow. They remain hypotheses:
  // changed results can still be semantically significant.
  if (differing.length > 64) return {kind:'limited'};
  const rules = new Map();
  for (const {expected: a, actual: b} of differing) {
    if (Buffer.byteLength(a.text) > 16384 || Buffer.byteLength(b.text) > 16384
        || Buffer.byteLength(JSON.stringify(b.tool)) > 16384) return {kind:'limited'};
    const key = JSON.stringify([b.tool, b.text]);
    if (rules.has(key) && rules.get(key) !== a.text) return null;
    rules.set(key, a.text);
  }
  const transform = (body) => normalizeToolResults(body, ({text, tool}) =>
    rules.get(JSON.stringify([tool, text])) ?? text);
  if (!sameRequest({...expected, body: transform(expected.body)}, {...actual, body: transform(actual.body)})) return null;
  // Escape letters in literal data so source containing text like "import x"
  // cannot be mistaken for an executable import by the worker's ABI check.
  const entries = [...rules];
  const source = temporaryNormalizerSource(entries);
  return source ? { kind: "temporary", source, entries } : {kind:'limited'};
}
