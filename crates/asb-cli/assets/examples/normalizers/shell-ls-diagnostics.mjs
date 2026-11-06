// Opt in where merged stdout/stderr from ls has unstable stream ordering.
export function normalizeToolResult({ text, tool }) {
  if (!["bash", "Bash", "shell_command"].includes(tool?.name) || typeof tool.arguments?.command !== "string"
      || !/(?:^|[\s;&|])ls(?:\s|$)/.test(tool.arguments.command)) return text;
  const lines = text.split("\n");
  const errors = lines.filter((line) => /^ls: cannot access '.+': /.test(line));
  if (!errors.length) return text;
  return [...errors.sort(), ...lines.filter((line) => !/^ls: cannot access '.+': /.test(line))].join("\n");
}
