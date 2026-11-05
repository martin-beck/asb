// Opt in for Python workloads where process-local repr addresses are known
// to be incidental; preserve class and callable names and all other text.
export function normalizeToolResult({ text, tool }) {
  if (tool?.name !== "bash" || typeof tool.arguments?.command !== "string"
      || !/(?:^|[\s;&|])(?:python(?:3(?:\.\d+)?)?|pytest)\b/.test(tool.arguments.command)) return text;
  return text
    .replace(/(<function [^\n]*? at )0x[0-9a-f]+>/gi, "$1<asb-address>>")
    .replace(/(<[^<>\n]+ object at )0x[0-9a-f]+>/gi, "$1<asb-address>>");
}
