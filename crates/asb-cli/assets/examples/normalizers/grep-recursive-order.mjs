// Opt in only when unordered recursive grep matches are equivalent for the task.
// Head-limited, ambiguous, multiline or diagnostic output is left untouched.
export function normalizeToolResult({ text, tool }) {
  if (!['Bash', 'bash', 'shell_command'].includes(tool?.name)) return text;
  const command = tool?.arguments?.command;
  if (typeof command !== 'string') return text;
  const match = /^grep -rn "[^"\r\n]+" ([A-Za-z0-9_./-]+\/) \| head -([1-9][0-9]*)$/.exec(command);
  if (!match || !text) return text;
  const limit = Number(match[2]);
  // Claude's Bash adapter strips the final newline. Preserve its absence;
  // other shell adapters still require a complete newline-terminated result.
  const trailing = text.endsWith('\n');
  if (!trailing && tool.name !== 'Bash') return text;
  const lines = (trailing ? text.slice(0, -1) : text).split('\n');
  if (!Number.isSafeInteger(limit) || lines.length >= limit) return text;
  const prefix = match[1];
  for (const line of lines) {
    if (!line.startsWith(prefix) || !/^[A-Za-z0-9_./-]+:[1-9][0-9]*:/.test(line)) return text;
    // A second path/line prefix can indicate a preceding unterminated file.
    if (/[A-Za-z0-9_./-]+\/[A-Za-z0-9_./-]+:[1-9][0-9]*:/.test(line.slice(prefix.length).replace(/^[A-Za-z0-9_./-]+:[1-9][0-9]*:/, ''))) return text;
  }
  return lines.sort().join('\n') + (trailing ? '\n' : '');
}
