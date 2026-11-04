// Goose assigns new session IDs on replay. Give only observed subagent IDs
// per-proxy stable aliases; retain distinct IDs and all other message content.
import { readFileSync } from 'node:fs';
const sessions = new Map();
let nextSession = 1;
const patterns = [
  /^Subagent ID: (\d{8}_\d+)/gm,
  /^Task (\d{8}_\d+) started in background/gm,
  /^# Background Task Result: (\d{8}_\d+)/gm,
  /^• (\d{8}_\d+):/gm,
];

function visit(value, fn) {
  if (typeof value === 'string') return fn(value);
  if (Array.isArray(value)) return value.map(item => visit(item, fn));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, visit(item, fn)]));
  return value;
}

export function normalizeRequest({ body }) {
  if (!body || typeof body !== 'object' || !Array.isArray(body.messages)) return body;
  if (process.env.ASB_GOOSE_ID_MAP_FILE) {
    try {
      const scopes = JSON.parse(readFileSync(process.env.ASB_GOOSE_ID_MAP_FILE, 'utf8'));
      for (const pairs of Object.values(scopes)) for (const [live, recorded] of Object.entries(pairs)) {
        const alias = sessions.get(live) || sessions.get(recorded) || `<asb-goose-session-${nextSession++}>`;
        if ((sessions.has(live) && sessions.get(live) !== alias) || (sessions.has(recorded) && sessions.get(recorded) !== alias)) throw new Error('Goose session alias conflict');
        sessions.set(live, alias);sessions.set(recorded, alias);
      }
    } catch (error) { if (error?.code !== 'ENOENT') throw error; }
  }
  visit(body.messages, text => {
    for (const pattern of patterns) {
      pattern.lastIndex = 0;
      for (const match of text.matchAll(pattern)) {
        if (!sessions.has(match[1])) sessions.set(match[1], `<asb-goose-session-${nextSession++}>`);
      }
    }
    return text;
  });
  const messages = body.messages.map(message => {
    const output = { ...message };
    if (Object.hasOwn(output, 'content')) output.content = visit(output.content, text => {
      if (!patterns.some(pattern => { pattern.lastIndex = 0; return pattern.test(text); })) return text;
      let normalized = text;
      for (const pattern of patterns) {
        pattern.lastIndex = 0;
        normalized = normalized.replace(pattern, (match, id) => match.replace(id, sessions.get(id) || id));
      }
      if (normalized.startsWith('Task <asb-goose-session-')) {
        normalized = normalized.replace(/load\(source: "(\d{8}_\d+)"\)\.$/, (match, id) => match.replace(id, sessions.get(id) || id));
      }
      normalized = normalized.replace(/^(• <asb-goose-session-\d+>:.* - running )\d+s(, \d+ turns, idle )\d+s/gm, '$1<duration>s$2<duration>s');
      if (normalized.startsWith('# Background Task Result: <asb-goose-session-')) {
        normalized = normalized.replace(/(\*\*Duration:\*\* )\d+s/g, '$1<duration>s');
      }
      return normalized;
    });
    if (Array.isArray(output.tool_calls)) output.tool_calls = output.tool_calls.map(tool => {
      if (!['load', 'summon__load'].includes(tool?.function?.name) || typeof tool.function.arguments !== 'string') return tool;
      try {
        const args = JSON.parse(tool.function.arguments);
        if (typeof args?.source !== 'string' || !sessions.has(args.source)) return tool;
        return { ...tool, function: { ...tool.function, arguments: JSON.stringify({ ...args, source: sessions.get(args.source) }) } };
      } catch { return tool; }
    });
    return output;
  });
  return { ...body, messages };
}
