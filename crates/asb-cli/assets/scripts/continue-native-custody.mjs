// Continue-owned bridge between a completed broker recording and ASB's native
// sidecar. Callers obtain digests from the actual program and settled workspace
// effect evidence; no default or synthetic effect digest is supplied here.
import { ContinueBroker } from './continue-native-broker.mjs';
import { encodeSidecar, decodeSidecar } from './continue-native-sidecar.mjs';

export async function sealBroker(broker, { programSha256, workspaceEffectSha256 }) {
  if (!(broker instanceof ContinueBroker) || broker.mode !== 'record') throw new TypeError('Continue recording broker is required');
  // Check digest shape before closing the broker, so callers can correct a
  // missing settlement without losing the live in-memory recording.
  for (const value of [programSha256, workspaceEffectSha256]) {
    if (typeof value !== 'string' || !/^[a-f0-9]{64}$/.test(value)) throw new Error('Continue sidecar requires actual program and workspace-effect digests');
  }
  const encoded = encodeSidecar(broker.session.preview(), programSha256, workspaceEffectSha256);
  await broker.finish();
  return encoded;
}

export function replayBroker(sidecar, { programSha256, workspaceEffectSha256, applyEffects }) {
  const decoded = decodeSidecar(sidecar);
  if (decoded.programSha256 !== programSha256 || decoded.workspaceEffectSha256 !== workspaceEffectSha256) {
    throw new Error('Continue sidecar does not match the selected program and workspace effects');
  }
  if (typeof applyEffects !== 'function') throw new TypeError('Continue replay requires workspace-effect application');
  return new ContinueBroker('replay', decoded.exchanges, applyEffects);
}
