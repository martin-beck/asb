// Preserve legacy workspace keys without retrying path searches at every byte.
export function normalizeWorkspacePaths(body, replacement = "<asb-workspace>") {
  const runs = /\/?[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)*/g;
  const guestSnapshot = /^\/?asb-snapshot-storage\/snapshot\/filesystem\/workspace(?:\/|$)/;
  return body
    .replace(runs, (run) => guestSnapshot.test(run) ? run : run.replace(/^\/?(?:[A-Za-z0-9_.-]+\/)+workspace-concurrency-\d+(?:\.asb-agent-runtime)?/, replacement))
    .replace(runs, (run, offset, source) => {
      if (guestSnapshot.test(run)) return run;
      const following = source[offset + run.length];
      const suffix = following === "/" || following === '"' || following === "\\" ? following : "";
      const normalized = (run + suffix).replace(/^\/?(?:[A-Za-z0-9_.-]+\/)+workspace(?=\/|["\\])/, replacement);
      return suffix ? normalized.slice(0, -1) : normalized;
    });
}

// A streamed path can cross JSON event boundaries. Reassemble each logical
// delta field before rewriting paths, then distribute it back across the same
// events. Encode slashes in SSE JSON so a later whole-body pass cannot mistake
// an isolated suffix for a complete relative path; JSON clients decode them.
export function normalizeOpenAiSseWorkspacePaths(body, workspace, replacement = "<asb-workspace>") {
  if (!body.includes("data:")) return body;
  const lines = body.split(/(?<=\n)/);
  const events = new Map();
  const groups = new Map();
  const add = (key, object, field) => {
    if (typeof object?.[field] !== "string") return;
    const group = groups.get(key) || [];
    group.push({ object, field, value: object[field] });
    groups.set(key, group);
  };
  const collect = (value, key) => {
    if (Array.isArray(value)) {
      for (const [position, item] of value.entries()) {
        const index = Number.isInteger(item?.index) ? item.index : position;
        if (typeof item === "string") add(`${key}[${index}]`, value, position);
        else collect(item, `${key}[${index}]`);
      }
    } else if (value && typeof value === "object") {
      for (const [field, item] of Object.entries(value)) {
        if (typeof item === "string") add(`${key}.${field}`, value, field);
        else collect(item, `${key}.${field}`);
      }
    }
  };
  for (let index = 0; index < lines.length; index += 1) {
    const match = lines[index].match(/^(data:\s+)(\{.*\})(\r?\n)?$/);
    if (!match) continue;
    let event;
    try { event = JSON.parse(match[2]); } catch { continue; }
    if (!Array.isArray(event.choices)) continue;
    events.set(index, {event, prefix: match[1], newline: match[3] || ""});
    for (const [position, choice] of event.choices.entries()) {
      const delta = choice?.delta;
      if (!delta || typeof delta !== "object") continue;
      const id = Number.isInteger(choice.index) ? choice.index : position;
      collect(delta, `${id}:delta`);
    }
  }
  for (const group of groups.values()) {
    const joined = group.map((part) => part.value).join("");
    const normalized = normalizeWorkspacePaths(workspace ? joined.replaceAll(workspace, replacement) : joined, replacement);
    let cursor = 0;
    for (let index = 0; index < group.length; index += 1) {
      let end = index === group.length - 1 ? normalized.length : Math.min(normalized.length, cursor + group[index].value.length);
      // A changed path length can shift a boundary into a surrogate pair.
      if (end > cursor && end < normalized.length && /[\uD800-\uDBFF]/.test(normalized[end - 1]) && /[\uDC00-\uDFFF]/.test(normalized[end])) end += 1;
      group[index].object[group[index].field] = normalized.slice(cursor, end);
      cursor = end;
    }
  }
  for (const [index, item] of events) {
    lines[index] = `${item.prefix}${JSON.stringify(item.event).replaceAll("/", "\\u002f")}${item.newline}`;
  }
  return lines.join("");
}
