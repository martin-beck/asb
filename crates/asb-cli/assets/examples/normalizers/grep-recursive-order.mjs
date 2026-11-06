// Opt in only when unordered recursive grep matches are equivalent for the task.
// Compare only the visible matches; changed selections, ambiguous, multiline
// and diagnostic output remain significant.
export function normalizeToolResult({ text, tool }) {
  if (!['Bash', 'bash', 'shell_command'].includes(tool?.name)) return text;
  const command = tool?.arguments?.command;
  if (typeof command !== 'string') return text;
  const compound = /^(?:git config --global --add safe\.directory \/testbed; )?cd \/testbed && git log --oneline -([1-9][0-9]*) && (grep .*)$/.exec(command);
  const search = (compound ? compound[2] : command).replace(/ --include="(\*\.[A-Za-z0-9_-]+|[A-Za-z0-9_.-]+)"/, ' --include=$1');
  const match = /^grep (?:-rn|-n) "[^"\r\n]+"(?: -r)? ([A-Za-z0-9_.*\/-]+(?: [A-Za-z0-9_.*\/-]+)*)(?: --include=(\*\.[A-Za-z0-9_-]+|[A-Za-z0-9_.-]+))?(?: 2>\/dev\/null)?(?: \| grep -(?:v|iv|vi) "[^"\r\n]+")* \| head -([1-9][0-9]*)$/.exec(search);
  if (!match || !text) return text;
  const paths = match[1].split(' ');
  if (paths.some((path) => !path || path.includes('..') ||
      path.split('/').some((part) => part.includes('*') && !/^\*\.[A-Za-z0-9_-]+$/.test(part)))) return text;
  const allowed = paths.map((path) => {
    const escaped = path.replace(/[.]/g, '\\.').replace(/\*/g, '[^/]*');
    return new RegExp('^' + escaped + (path.endsWith('/') ? '.+' : '') + '$');
  });
  const include = match[2] && new RegExp('^' + match[2].replace(/[.]/g, '\\.').replace(/\*/g, '[^/]*') + '$');
  const limit = Number(match[3]);
  // Claude's Bash adapter strips the final newline. Preserve its absence;
  // other shell adapters still require a complete newline-terminated result.
  const trailing = text.endsWith('\n');
  if (!trailing && tool.name !== 'Bash') return text;
  const lines = (trailing ? text.slice(0, -1) : text).split('\n');
  if (compound && !Number.isSafeInteger(Number(compound[1]))) return text;
  const prefix = compound ? lines.splice(0, Number(compound[1])) : [];
  if (compound && (prefix.length !== Number(compound[1]) || prefix.some((line) => !/^[0-9a-f]{7,64} .+/.test(line)))) return text;
  if (!Number.isSafeInteger(limit) || lines.length > limit) return text;
  for (const line of lines) {
    const entry = /^([A-Za-z0-9_./-]+):[1-9][0-9]*:/.exec(line);
    if (!entry || !allowed.some((path) => path.test(entry[1])) ||
        (include && !include.test(entry[1].split('/').at(-1)))) return text;
    // A second path/line prefix can indicate a preceding unterminated file.
    if (/[A-Za-z0-9_./-]+\/[A-Za-z0-9_./-]+:[1-9][0-9]*:/.test(line.slice(entry[0].length))) return text;
  }
  return [...prefix, ...lines.sort()].join('\n') + (trailing ? '\n' : '');
}
