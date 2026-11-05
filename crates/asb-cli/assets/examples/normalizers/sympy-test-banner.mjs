// Opt-in example for SymPy test output. Configure the profile only for the
// intended SWE-bench family and SymPy task IDs. The seed can affect test
// behavior; replacing its printed value does not make a run deterministic.
export function normalizeToolResult({ text, tool }) {
  if (tool?.name !== "bash" || typeof tool.arguments?.command !== "string"
      || !tool.arguments.command.includes("sympy.test(")) return text;
  const starts = [...text.matchAll(/^=+ test process starts =+\r?$/gm)];
  const finishes = [...text.matchAll(/^=+ tests finished: .+\r?$/gm)];
  const seeds = [...text.matchAll(/^(random seed:[ \t]*)\d+(?=\r?$)/gm)];
  if (starts.length !== 1 || finishes.length !== 1 || seeds.length !== 1
      || starts[0].index >= seeds[0].index || seeds[0].index >= finishes[0].index) return text;
  return text.replace(/^(random seed:[ \t]*)\d+(?=\r?$)/m, "$1<asb-seed>");
}
