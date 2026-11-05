// Opt in when pip download timing varies in this agent/workload combination.
export function normalizeToolResult({ text, tool }) {
  if (tool?.name !== "bash" || typeof tool.arguments?.command !== "string"
      || !/(?:^|[\s;&|])(?:pip(?:3(?:\.\d+)?)?|python(?:3(?:\.\d+)?)?\s+-m\s+pip)\b/.test(tool.arguments.command)) return text;
  return text.replace(/(\b\d+(?:\.\d+)?\/\d+(?:\.\d+)? (?:kB|MB|GB) )\d+(?:\.\d+)? (?:kB|MB|GB)\/s eta \d+:\d+:\d+/g, "$1<asb-transfer-rate> eta <asb-transfer-eta>");
}
