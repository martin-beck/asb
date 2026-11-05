// Opt in only for agents and workloads where a Python unittest command emits
// this complete summary. Counts and results remain part of the comparison.
export function normalizeToolResult({ text, tool }) {
  if (tool?.name !== "bash" || typeof tool.arguments?.command !== "string"
      || !/\b(?:python(?:3(?:\.\d+)?)?\s+-m\s+unittest|unittest)\b/.test(tool.arguments.command)) return text;
  return text.replace(/(^|\r?\n)(Ran \d+ tests? in )\d+(?:\.\d+)?s\b(?=(?:\r?\n){2}(?:OK|FAILED(?: \([^\r\n]*\))?)(?:\r?\n|$))/g, "$1$2<asb-elapsed>");
}
