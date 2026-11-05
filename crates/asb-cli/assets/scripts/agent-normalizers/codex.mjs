// Codex-only replay identity cleanup.  This module is selected by the Codex
// adapter and is intentionally limited to known protocol metadata and tool
// result envelopes; prompts and tool arguments remain strict cassette input.
function normalizeUnittestElapsed(value) {
  return value.replace(/(^|\\n|\r?\n)(Ran \d+ tests? in )\d+(?:\.\d+)?s\b(?=(?:\\n|\r?\n){2}(?:OK|FAILED(?: \([^\r\n]*\))?)(?:\\n|\r?\n|$))/g, "$1$2<asb-elapsed>");
}
function normalizeDiffMtime(value) {
  return value.replace(/(^|\\n|\r?\n)((?:---|\+\+\+) [^\r\n\t]+\t)\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d+)? [+-]\d{4}\b/g, "$1$2<asb-diff-mtime>");
}
function normalizeSympySeed(value) {
  const starts = [...value.matchAll(/^=+ test process starts =+\r?$/gm)];
  const finishes = [...value.matchAll(/^=+ tests finished: .+\r?$/gm)];
  const seeds = [...value.matchAll(/^(random seed:[ \t]*)\d+(?=\r?$)/gm)];
  return starts.length === 1 && finishes.length === 1 && seeds.length === 1
      && starts[0].index < seeds[0].index && seeds[0].index < finishes[0].index
    ? value.replace(/^(random seed:[ \t]*)\d+(?=\r?$)/m, "$1<asb-seed>") : value;
}
export function normalizeRequest(request) {
  const root = request?.body;
  if (!root || typeof root !== "object" || Array.isArray(root)) return root;
  const visit = (value, path = []) => {
    if (Array.isArray(value)) return value.map((item, index) => visit(item, [...path, String(index)]));
    if (!value || typeof value !== "object") return value;
    const result = {};
    for (const [key, raw] of Object.entries(value)) {
      const next = [...path, key];
      if (path.length === 1 && path[0] === "client_metadata" && [
        "root_turn_id", "session_id", "thread_id", "turn_id",
        "x-codex-installation-id", "x-codex-turn-metadata", "x-codex-window-id",
      ].includes(key)) {
        result[key] = "<asb-codex-client-metadata>";
      } else if (path.length === 2 && path[0] === "input" && /^\d+$/.test(path[1])
          && key === "encrypted_content") {
        result[key] = "<asb-codex-encrypted-content>";
      } else if (path.length === 2 && path[0] === "input" && /^\d+$/.test(path[1])
          && key === "call_id") {
        result[key] = "<asb-codex-input-call-id>";
      } else if (path.length === 2 && path[0] === "input" && /^\d+$/.test(path[1])
          && key === "output" && typeof raw === "string") {
        result[key] = raw
          .replace(/^(Tool (?:result|time): )\d+(?:\.\d+)? seconds(?=\r?\n)/, "$1<asb-tool-time> seconds")
          .replace(/^(Wall time: )\d+(?:\.\d+)? seconds(?=\r?\nOutput:)/, "$1<asb-wall-time> seconds")
          .replace(/Original token count: \d+/g, "Original token count: <asb-token-count>")
          .replace(/original token count: \d+/g, "original token count: <asb-token-count>")
          .replace(/Total output lines: \d+/g, "Total output lines: <asb-line-count>")
          .replace(/codex-arg[A-Za-z0-9_-]+/g, "codex-arg<asb-random>")
          .replace(/(Chunk ID: [^\n]*?\r?\nWall time: )\d+(?:\.\d+)? seconds(?=\r?\nProcess exited with code \d+)/g, "$1<asb-wall-time> seconds")
          .replace(/(Chunk ID: [^\n]*?\r?\nWall time: )\d+(?:\.\d+)? seconds(\r?\nProcess running with session ID )\d+/g, "$1<asb-wall-time> seconds$2<asb-session>")
          .replace(/(Tool result: )\d+(?:\.\d+)? seconds(?=\r?\nMetadata:)/g, "$1<asb-tool-time> seconds");
        result[key] = normalizeSympySeed(normalizeDiffMtime(normalizeUnittestElapsed(result[key])));
      } else {
        result[key] = visit(raw, next);
      }
    }
    return result;
  };
  return visit(root);
}
