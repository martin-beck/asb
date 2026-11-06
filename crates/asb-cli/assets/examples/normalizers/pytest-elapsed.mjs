// Select only for agent/workload combinations that invoke pytest. Keep test
// counts, statuses, diagnostic content and command arguments significant.
export function normalizeToolResult({ text, tool }) {
  if (tool?.name !== "bash" || typeof tool.arguments?.command !== "string"
      || !/(?:^|[\s;&|])(?:pytest\b|python(?:3(?:\.\d+)?)?\s+-m\s+pytest\b)/.test(tool.arguments.command)) return text;
  return text
    .replace(/(^|\r?\n)((?:\d+ (?:failed|passed|skipped|deselected|errors?|warnings?),? )+in )\d+(?:\.\d+)?s\b/g, "$1$2<asb-elapsed>")
    .replace(/((?:\d+ (?:passed|failed|skipped|deselected|errors?|warnings?)(?:, )?)+(?:\u001b\[(?:0|3[12])m)* in )\d+(?:\.\d+)?s(?=\u001b\[0m)/g, "$1<asb-pytest-elapsed>");
}
