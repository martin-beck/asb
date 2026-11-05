// Configure only for the OpenCode agent and workloads known to enumerate
// repository search matches in nondeterministic filesystem order.
export function normalizeToolResult({ text, tool }) {
  if (tool?.name !== "grep") return text;
  const header = text.match(/^(Found \d+ matches\n)/);
  if (!header) return text;
  const body = text.slice(header[0].length);
  const trailing = body.endsWith("\n");
  const blocks = body.split(/\n{2,}(?=\/testbed\/)/).map((block) => block.replace(/\n+$/, ""));
  if (blocks.length < 2 || blocks.some((block) => !block.startsWith("/testbed/"))) return text;
  return header[0] + blocks.sort((left, right) => left.localeCompare(right)).join("\n\n") + (trailing ? "\n" : "");
}
