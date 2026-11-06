// Exploratory request comparison. Candidate functions see only protocol-owned
// tool-result text and associated tool metadata, never the whole request.
import { normalizeToolResults } from "./tool-result-normalization.mjs";

function results(body) {
  const found = [];
  normalizeToolResults(body, ({ text, tool, location }) => {
    found.push({ text, tool, location });
    return text;
  });
  return found;
}

function sameRequest(expected, actual) {
  return expected.method === actual.method && expected.url === actual.url
    && JSON.stringify(expected.body) === JSON.stringify(actual.body);
}

// A match here is evidence about this request only, not the rest of a replay.
// Candidates are supplied by the caller; never load code from a cassette.
export function findNormalizer(expected, actual, candidates) {
  if (expected.method !== actual.method || expected.url !== actual.url) return null;
  if (!expected.body || !actual.body || typeof expected.body !== "object" || typeof actual.body !== "object") return null;
  const left = results(expected.body);
  const right = results(actual.body);
  if (left.length !== right.length || left.length > 4096) return null;
  if (left.some((entry, index) => entry.location !== right[index].location
      || JSON.stringify(entry.tool) !== JSON.stringify(right[index].tool))) return null;

  for (const candidate of candidates) {
    let changed = false;
    try {
      const transform = (body) => normalizeToolResults(body, ({text, tool}) => {
        const value = candidate.normalizeToolResult({text, tool});
        if (typeof value !== "string") throw new TypeError("normalizer returned non-string");
        changed ||= value !== text;
        return value;
      });
      const normalizedExpected = {...expected, body: transform(expected.body)};
      const normalizedActual = {...actual, body: transform(actual.body)};
      if (changed && sameRequest(normalizedExpected, normalizedActual)) {
        return { kind: "existing", id: candidate.id };
      }
    } catch {
      // An unavailable diagnostic is not evidence that replay matches.
    }
  }

  // An exact observed-pair rule is deliberately narrow. It is only a
  // hypothesis: the changed result can still be semantically significant.
  const differing = left.map((entry, index) => ({expected: entry, actual: right[index]}))
    .filter(({expected: a, actual: b}) => a.text !== b.text);
  if (differing.length !== 1 || !differing[0].expected.tool) return null;
  const {expected: a, actual: b} = differing[0];
  if (Buffer.byteLength(a.text) > 16384 || Buffer.byteLength(b.text) > 16384) return null;
  const transform = (body) => normalizeToolResults(body, ({text, tool}) =>
    JSON.stringify(tool) === JSON.stringify(b.tool) && text === b.text ? a.text : text);
  if (!sameRequest({...expected, body: transform(expected.body)}, {...actual, body: transform(actual.body)})) return null;
  const source = `export function normalizeToolResult({text, tool}) {\n  if (JSON.stringify(tool) === ${JSON.stringify(JSON.stringify(b.tool))} && text === ${JSON.stringify(b.text)}) return ${JSON.stringify(a.text)};\n  return text;\n}\n`;
  return { kind: "temporary", source };
}
