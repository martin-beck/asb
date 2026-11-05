import { readFile } from 'node:fs/promises';
import { createOpenCodeNativePlugin } from './opencode-native-plugin.mjs';
import { decodeSidecar } from './native-tool-sidecar.mjs';

export async function recordedDefinitions(cassette, sidecar) {
  const {exchanges} = decodeSidecar(await readFile(sidecar), 'opencode');
  const names = new Set(exchanges.map(event => event.request.tool).filter(name => name !== 'task'));
  const definitions = {};
  for (const line of (await readFile(cassette, 'utf8')).split('\n')) {
    if (!line.trim()) continue;
    const row = JSON.parse(line);
    let request; try { request = JSON.parse(row?.request?.body); } catch { continue; }
    for (const entry of request.tools ?? []) {
      const tool = entry.function ?? entry;
      const name = tool.name;
      const schema = tool.parameters ?? tool.input_schema;
      if (names.has(name) && schema && !Object.hasOwn(definitions, name)) {
        definitions[name] = {description:typeof tool.description === 'string' ? tool.description : '', schema};
      }
    }
    if (Object.keys(definitions).length === names.size) break;
  }
  const missing = [...names].filter(name => !Object.hasOwn(definitions, name));
  if (missing.length) throw new Error(`OpenCode recording lacks tool definitions for: ${missing.join(', ')}`);
  return definitions;
}

export default async function () {
  const mode = process.env.ASB_OPENCODE_NATIVE_MODE;
  const definitions = mode === 'replay' ? await recordedDefinitions(process.env.ASB_OPENCODE_CASSETTE, process.env.ASB_OPENCODE_SIDECAR) : {};
  return createOpenCodeNativePlugin({mode, socket:process.env.ASB_OPENCODE_SOCKET, marker:process.env.ASB_OPENCODE_MARKER, directory:process.env.ASB_OPENCODE_STATE, definitions});
}
