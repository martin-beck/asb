// Opt-in for SymPy SWE-bench tasks. Matching output does not guarantee that
// different random seeds caused identical execution or make a run deterministic.
export function normalizeToolResult({ text, tool }) {
  const command = tool?.arguments?.command;
  if (!['bash', 'shell_command'].includes(tool?.name) || typeof command !== 'string'
      || !(command.includes('sympy.test(') || /(?:^|[\s;&|])(?:python(?:\d+(?:\.\d+)*)?\s+)?(?:\.\/|\/testbed\/)?bin\/test(?:\s|$)/.test(command))) return text;
  const starts = [...text.matchAll(/^=+ test process starts =+\r?$/gm)];
  const finishes = [...text.matchAll(/^=*[ \t]*tests finished: .+?=+[ \t]*\r?$/gm)];
  const elapsed = /^=*[ \t]*tests finished: ((?:\d+ (?:passed|failed|skipped|expected to fail|unexpectedly passed|errors?)(?:, )?)+)in (?:\d+(?:\.\d+)?|<asb-elapsed>) seconds[ \t]+=+[ \t]*\r?$/;
  const seeds = [...text.matchAll(/^(random seed:[ \t]*)(?:\d+|<asb-seed>)(?=\r?$)/gm)];
  const seedLines = [...text.matchAll(/^random seed:/gm)];
  // tail may remove the start banner, but only accept an exact contiguous
  // metadata suffix with an optional executable line, not an arbitrary seed.
  const tailed = /^(?:executable:[ \t]+\/[^\r\n]+[ \t]+\([^\r\n]+\)[ \t]+\[[^\r\n]+\]\r?\n)?architecture:[ \t]+[^\r\n]+\r?\ncache:[ \t]+[^\r\n]+\r?\nground types:[ \t]+[^\r\n]+\r?\nnumpy:[ \t]+[^\r\n]+\r?\nrandom seed:[ \t]*(?:\d+|<asb-seed>)\r?\nhash randomization:[ \t]+off\r?\n/.test(text);
  if (finishes.length !== 1 || starts.length > 1 || seeds.length > 1) return text;
  const seedSafe = seeds.length === 1 && seedLines.length === 1 && seeds[0].index < finishes[0].index
      && (starts.length === 1 ? starts[0].index < seeds[0].index : tailed && elapsed.test(finishes[0][0]));
  const withSeed = seedSafe ? text.replace(/^(random seed:[ \t]*)\d+(?=\r?$)/m, '$1<asb-seed>') : text;
  return withSeed.replace(/^=*[ \t]*tests finished: ((?:\d+ (?:passed|failed|skipped|expected to fail|unexpectedly passed|errors?)(?:, )?)+)in \d+(?:\.\d+)? seconds[ \t]+=+[ \t]*(?=\r?$)/m,
      '=== tests finished: $1in <asb-elapsed> seconds ===');
}
