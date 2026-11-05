// Claude gives delegated tasks fresh IDs and output paths in each execution.
// Only hook-observed pairs may be aliased; unrelated task content remains exact.
const ids = new Map(), paths = new Map(), liveToRecorded = new Map(), recordedToLive = new Map(), taskPaths = new Map();
let next = 0;
function bind(map, live, recorded, alias) {
  if (typeof live !== 'string' || !live || typeof recorded !== 'string' || !recorded)
    throw new Error('Claude task mapping is invalid');
  const existing = map.get(live) || map.get(recorded);
  if (existing && existing !== alias) throw new Error('Claude task mapping conflicts');
  map.set(live, alias); map.set(recorded, alias);
}
export function controlRequest({method, url, body, mode}) {
  if (url !== '/.asb/claude-task-map') return undefined;
  if (mode !== 'replay' || method !== 'POST' || body.length > 8192) return false;
  const value = JSON.parse(body.toString('utf8'));
  if (!value || Object.keys(value).sort().join(',') !== 'live_id,live_path,recorded_id,recorded_path' ||
      !/^a[0-9a-f]+$/.test(value.live_id) || !/^a[0-9a-f]+$/.test(value.recorded_id) ||
      typeof value.live_path !== 'string' || typeof value.recorded_path !== 'string' ||
      !value.live_path.endsWith(`/tasks/${value.live_id}.output`) ||
      !value.recorded_path.endsWith(`/tasks/${value.recorded_id}.output`)) return false;
  const existingLive = liveToRecorded.get(value.live_id), existingRecorded = recordedToLive.get(value.recorded_id);
  if ((existingLive && existingLive !== value.recorded_id) || (existingRecorded && existingRecorded !== value.live_id) ||
      (!existingLive && (ids.has(value.live_id) || ids.has(value.recorded_id))))
    throw new Error('Claude task mapping is not one-to-one');
  const previousPaths=taskPaths.get(value.live_id);
  if (previousPaths && (previousPaths[0] !== value.live_path || previousPaths[1] !== value.recorded_path))
    throw new Error('Claude task output path changed');
  const alias = ids.get(value.live_id) || ids.get(value.recorded_id) || `<asb-claude-task-${++next}>`;
  const pathAlias = `${alias}-output`;
  if ((paths.has(value.live_path) && paths.get(value.live_path) !== pathAlias) ||
      (paths.has(value.recorded_path) && paths.get(value.recorded_path) !== pathAlias))
    throw new Error('Claude task path mapping conflicts');
  bind(ids,value.live_id,value.recorded_id,alias);
  bind(paths,value.live_path,value.recorded_path,pathAlias);
  liveToRecorded.set(value.live_id,value.recorded_id);
  recordedToLive.set(value.recorded_id,value.live_id);
  taskPaths.set(value.live_id,[value.live_path,value.recorded_path]);
  return true;
}
function normalizeText(text) {
  return text.replace(/<task-notification>\n[\s\S]*?\n<\/task-notification>/g, block => {
    const id=block.match(/<task-id>([^<]+)<\/task-id>/)?.[1];
    const path=block.match(/<output-file>([^<]+)<\/output-file>/)?.[1];
    if (!id || !path || !ids.has(id) || !paths.has(path)) return block;
    return block.replace(`<task-id>${id}</task-id>`,`<task-id>${ids.get(id)}</task-id>`)
      .replace(`<output-file>${path}</output-file>`,`<output-file>${paths.get(path)}</output-file>`)
      .replace(/(<usage>(?:(?!<\/usage>)[\s\S])*?<duration_ms>)\d+(<\/duration_ms>)/g,'$1<asb-duration>$2');
  });
}
function visit(value) {
  if (typeof value === 'string') return normalizeText(value);
  if (Array.isArray(value)) return value.map(visit);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key,item])=>[key,visit(item)]));
  return value;
}
export function normalizeRequest(request) {
  const body=request?.body;
  if (!body || typeof body !== 'object') return body;
  if (Array.isArray(body.system)) for (const block of body.system) {
    if (block?.type === 'text' && typeof block.text === 'string')
      block.text=block.text.replace(/^(CWD: [^\n]+\nDate: )\d{4}-\d{2}-\d{2}(?=\n|$)/,'$1<asb-claude-date>');
  }
  if (Array.isArray(body.messages)) body.messages=visit(body.messages);
  return body;
}
