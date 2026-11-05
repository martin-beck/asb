// Opt in for the relevant agent/workload combination. Normalize only complete
// unified-diff timestamps produced by an explicitly associated diff command.
export function normalizeToolResult({ text, tool }) {
  if (tool?.name !== "bash" || typeof tool.arguments?.command !== "string"
      || !/(?:^|[\s;&|])diff\s+[^\n]*(?:-u\b|--unified\b)/.test(tool.arguments.command)) return text;
  return text.replace(/(^|\r?\n)((?:---|\+\+\+) [^\r\n\t]+\t)\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d+)? [+-]\d{4}\b/g, "$1$2<asb-diff-mtime>");
}
