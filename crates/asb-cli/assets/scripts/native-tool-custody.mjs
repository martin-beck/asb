// Native tool-owned bridge between a completed broker recording and ASB's native
// sidecar. Callers obtain digests from the actual program and settled workspace
// effect evidence; no default or synthetic effect digest is supplied here.
import { NativeToolBroker } from './native-tool-broker.mjs';
import { encodeSidecar, encodePendingSidecar, decodeSidecar } from './native-tool-sidecar.mjs';

// Private intermediate for the host recorder. It cannot be published as a
// native sidecar until the settled workspace archive has been measured.
export async function sealPendingBroker(broker, { programSha256, agent = 'continue' }) {
  if (!(broker instanceof NativeToolBroker) || broker.mode !== 'record') throw new TypeError('Native tool recording broker is required');
  if (typeof programSha256 !== 'string' || !/^[a-f0-9]{64}$/.test(programSha256)) throw new Error('Native tool sidecar requires actual program digest');
  return broker.finish(events => encodePendingSidecar(events, programSha256, agent));
}

export async function sealBroker(broker, { programSha256, workspaceEffectSha256 }) {
  if (!(broker instanceof NativeToolBroker) || broker.mode !== 'record') throw new TypeError('Native tool recording broker is required');
  // Check digest shape before closing the broker, so callers can correct a
  // missing settlement without losing the live in-memory recording.
  for (const value of [programSha256, workspaceEffectSha256]) {
    if (typeof value !== 'string' || !/^[a-f0-9]{64}$/.test(value)) throw new Error('Native tool sidecar requires actual program and workspace-effect digests');
  }
  return broker.finish(events => encodeSidecar(events, programSha256, workspaceEffectSha256));
}

export function replayBroker(sidecar, { programSha256, workspaceEffectSha256, applyEffects, agent = 'continue' }) {
  const decoded = decodeSidecar(sidecar, agent);
  if (decoded.programSha256 !== programSha256 || decoded.workspaceEffectSha256 !== workspaceEffectSha256) {
    throw new Error('Native tool sidecar does not match the selected program and workspace effects');
  }
  if (typeof applyEffects !== 'function') throw new TypeError('Native tool replay requires workspace-effect application');
  return new NativeToolBroker('replay', decoded.exchanges, applyEffects, {agent});
}
