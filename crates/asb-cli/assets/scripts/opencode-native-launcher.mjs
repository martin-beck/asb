import { readFile } from 'node:fs/promises';
import { createOpenCodeNativePlugin } from './opencode-native-plugin.mjs';
import { decodeSidecar } from './native-tool-sidecar.mjs';

export async function recordedDefinitions(evidence, sidecar) {
  const {exchanges} = decodeSidecar(await readFile(sidecar), 'opencode');
  const names = new Set(exchanges.map(event => event.request.tool).filter(name => name !== 'task'));
  const available = JSON.parse(await readFile(evidence, 'utf8'));
  const definitions = {};
  for (const name of names) if (Object.hasOwn(available, name)) definitions[name] = available[name];
  const missing = [...names].filter(name => !Object.hasOwn(definitions, name));
  if (missing.length) throw new Error(`OpenCode recording lacks tool definitions for: ${missing.join(', ')}`);
  return definitions;
}

export default async function () {
  const mode = process.env.ASB_OPENCODE_NATIVE_MODE;
  const definitions = mode === 'replay' ? await recordedDefinitions(process.env.ASB_OPENCODE_EVIDENCE, process.env.ASB_OPENCODE_SIDECAR) : {};
  return createOpenCodeNativePlugin({mode, socket:process.env.ASB_OPENCODE_SOCKET, marker:process.env.ASB_OPENCODE_MARKER, directory:process.env.ASB_OPENCODE_STATE, definitions});
}
