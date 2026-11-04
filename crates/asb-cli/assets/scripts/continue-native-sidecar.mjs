// Continue's private codec for the common sealed native-tool sidecar format.
// The caller must supply digests from the actual launched program and the
// completed workspace-effect capture; this module does not invent evidence.
import { createHash } from 'node:crypto';
import { ReplaySession } from './continue-native-session.mjs';

const LIMIT = 512 * 1024;
const TOTAL = 4 * 1024 * 1024;
const MAX_EVENTS = 8192;
const MAX_SIDECAR = 16 * 1024 * 1024;
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const validDigest = value => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value);
const keys = (value, expected) => value && typeof value === 'object' && !Array.isArray(value) &&
  Object.keys(value).sort().join(',') === [...expected].sort().join(',');

export function encodeSidecar(exchanges, programSha256, workspaceEffectSha256) {
  if (!validDigest(programSha256) || !validDigest(workspaceEffectSha256)) throw new Error('Continue sidecar requires real program and workspace-effect digests');
  const prefix = encodePendingSidecar(exchanges, programSha256);
  const output = Buffer.concat([prefix, Buffer.from(JSON.stringify({event:'workspace_effect',sha256:workspaceEffectSha256})+'\n')]);
  if (output.length > MAX_SIDECAR) throw new Error('Continue sidecar exceeds supported bounds');
  return output;
}

// Private intermediate only: it is not a valid sidecar until the host binds
// it to the actual settled snapshot archive digest.
export function encodePendingSidecar(exchanges, programSha256) {
  if (!validDigest(programSha256)) throw new Error('Continue sidecar requires a real program digest');
  if (!Array.isArray(exchanges) || !exchanges.length || exchanges.length * 3 + 4 > MAX_EVENTS) throw new Error('Continue sidecar exchange count is invalid');
  // Reuse replay's ordering and shape validation before writing any bytes.
  new ReplaySession(exchanges);
  if (exchanges.some(exchange => !keys(exchange,['ordinal','request','outcome']) ||
      (exchange.outcome.ok ? !keys(exchange.outcome,['ok','value']) : !keys(exchange.outcome,['ok','name','message'])))) {
    throw new Error('Continue sidecar exchange contains unsupported fields');
  }
  const session = 'continue-launch-1', alias = 'continue-tools-1';
  const events = [{event:'launch',session,program_sha256:programSha256},{event:'session_alias',alias,session}];
  let total = 0;
  for (const exchange of exchanges) {
    const request = Buffer.from(JSON.stringify({ordinal:exchange.ordinal,request:exchange.request}));
    const output = Buffer.from(JSON.stringify(exchange.outcome));
    total += request.length + output.length;
    if (request.length > LIMIT || output.length > LIMIT || total > TOTAL) throw new Error('Continue sidecar payload exceeds supported bounds');
    events.push({event:'poll',alias},{event:'write',alias,request_sha256:digest(request),request_b64:request.toString('base64')},
      {event:'output',alias,response_sha256:digest(output),output_b64:output.toString('base64')});
  }
  events.push({event:'exit',alias,success:true});
  const output = Buffer.from(events.map(event=>JSON.stringify(event)).join('\n')+'\n');
  if (output.length > MAX_SIDECAR) throw new Error('Continue sidecar exceeds supported bounds');
  return output;
}

export function decodeSidecar(bytes) {
  if (!Buffer.isBuffer(bytes) || bytes.length > MAX_SIDECAR || !bytes.length || bytes.at(-1) !== 10) throw new Error('Continue sidecar framing is invalid');
  let events;
  try { events = bytes.toString('utf8').trimEnd().split('\n').map(line => JSON.parse(line)); }
  catch { throw new Error('Continue sidecar contains invalid JSON'); }
  if (events.length < 7 || events.length > MAX_EVENTS || (events.length - 4) % 3 !== 0) throw new Error('Continue sidecar event count is invalid');
  const launch = events[0], session = events[1], exit = events.at(-2), effect = events.at(-1);
  if (!keys(launch,['event','session','program_sha256']) || launch.event !== 'launch' || launch.session !== 'continue-launch-1' || !validDigest(launch.program_sha256) ||
      !keys(session,['event','alias','session']) || session.event !== 'session_alias' || session.alias !== 'continue-tools-1' || session.session !== launch.session ||
      !keys(exit,['event','alias','success']) || exit.event !== 'exit' || exit.alias !== session.alias || exit.success !== true ||
      !keys(effect,['event','sha256']) || effect.event !== 'workspace_effect' || !validDigest(effect.sha256)) throw new Error('Continue sidecar lifecycle is invalid');
  let total = 0; const exchanges = [];
  function payload(encoded, sha) {
    if (typeof encoded !== 'string' || !validDigest(sha)) throw new Error('Continue sidecar payload is invalid');
    const value = Buffer.from(encoded, 'base64');
    total += value.length;
    if (value.toString('base64') !== encoded || value.length > LIMIT || total > TOTAL || digest(value) !== sha) throw new Error('Continue sidecar payload failed verification');
    try { return JSON.parse(value.toString('utf8')); } catch { throw new Error('Continue sidecar payload JSON is invalid'); }
  }
  for (let i=2;i<events.length-2;i+=3) {
    const [poll, write, output] = events.slice(i,i+3);
    if (!keys(poll,['event','alias']) || poll.event !== 'poll' || poll.alias !== session.alias ||
        !keys(write,['event','alias','request_sha256','request_b64']) || write.event !== 'write' || write.alias !== session.alias ||
        !keys(output,['event','alias','response_sha256','output_b64']) || output.event !== 'output' || output.alias !== session.alias) throw new Error('Continue sidecar event order is invalid');
    const request = payload(write.request_b64,write.request_sha256), outcome = payload(output.output_b64,output.response_sha256);
    if (!keys(request,['ordinal','request'])) throw new Error('Continue sidecar request is invalid');
    if (!(outcome?.ok ? keys(outcome,['ok','value']) : keys(outcome,['ok','name','message']))) throw new Error('Continue sidecar outcome is invalid');
    exchanges.push({ordinal:request.ordinal,request:request.request,outcome});
  }
  new ReplaySession(exchanges);
  return {exchanges,programSha256:launch.program_sha256,workspaceEffectSha256:effect.sha256};
}
