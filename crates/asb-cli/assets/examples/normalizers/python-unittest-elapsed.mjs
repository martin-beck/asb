// Opt in only for agents and workloads where a Python unittest command emits
// this complete summary. Counts and results remain part of the comparison.
export function normalizeToolResult({ text, tool }) {
  if (!["bash", "Bash", "shell_command"].includes(tool?.name) || typeof tool.arguments?.command !== "string") return text;
  const command = tool.arguments.command;
  const unittest = /\b(?:python(?:3(?:\.\d+)?)?\s+-m\s+unittest|unittest)\b/.test(command);
  // Django's test runner emits the same unittest summary. Require a Python
  // invocation of its conventional path, not a mention in another command.
  const django = /(?:^|[;&|]\s*)(?:\/[^\s;&|]+\/)?python(?:3(?:\.\d+)?)?\s+(?:\.\/|\/testbed\/)?tests\/runtests\.py(?=\s|$)/.test(command);
  if (!unittest && !django) return text;
  return text.replace(/(^|\r?\n)(Ran \d+ tests? in )\d+(?:\.\d+)?s\b(?=(?:\r?\n){2}(?:OK|FAILED(?: \([^\r\n]*\))?)(?:\r?\n|$))/g, "$1$2<asb-elapsed>");
}
