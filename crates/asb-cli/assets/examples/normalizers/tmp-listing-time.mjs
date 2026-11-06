// Opt in only for tasks where /tmp listing modification times are incidental.
// Preserve file names, permissions, owners, sizes, diagnostics and all other text.
export function normalizeToolResult({text, tool}) {
  const command = tool?.arguments?.command;
  if (!['Bash','bash','shell_command'].includes(tool?.name) || typeof command !== 'string') return text;
  const plain = /(?:^|[;&])\s*ls -la \/tmp(?:\s*\|\s*head(?:\s+-[1-9][0-9]*)?)?\s*(?:$|[;&])/.test(command);
  const download = /^ls -la \/tmp\/[A-Za-z0-9_-]+ 2>\/dev\/null; pip download [A-Za-z0-9_.=-]+ -d \/tmp\/[A-Za-z0-9_-]+ --no-deps 2>&1 \| tail -[1-9][0-9]*$/.test(command);
  if (!plain && !download) return text;
  const trailing = text.endsWith('\n');
  const lines = (trailing ? text.slice(0,-1) : text).split('\n');
  if (!/^total [0-9]+$/.test(lines[0] ?? '')) return text;
  const row = /^([bcdlps-][rwxStTs-]{9}[.+@]?\s+\d+\s+\S+\s+\S+\s+\d+\s+)([A-Z][a-z]{2}\s+\d{1,2}\s+(?:\d{2}:\d{2}|\d{4}))(\s+.+)$/;
  if (lines.length < 2) return text;
  if (plain && lines.slice(1).some(line => !row.test(line) && line !== 'Shell cwd was reset to /testbed')) return text;
  let end = 1;
  while (end < lines.length && row.test(lines[end])) end++;
  if (end === 1) return text;
  return lines.map((line, index) => index < end ? line.replace(row, '$1<asb-mtime>$3') : line).join('\n') + (trailing ? '\n' : '');
}
