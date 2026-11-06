#!/usr/bin/env node

// Small project-local HTTP recorder/replayer embedded in ASB. It supplies the
// bounded loopback protocol that ASB's tape module invokes.

import { appendFileSync, existsSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import http from "node:http";
import { performance } from "node:perf_hooks";
import { isAbsolute } from "node:path";
import { pathToFileURL } from "node:url";
import { quiesceNetworkRequests, networkFetch } from "./network-fetch.mjs";
import { CaptureSession, completedSseResponse, responseDelivery } from "./capture-session.mjs";
import { normalizeOpenAiSseWorkspacePaths, normalizeWorkspacePaths, replaceExactWorkspace } from "./tape-normalization.mjs";
import { RequestTracing } from "./request-tracing.mjs";
import { normalizeToolResultsAsync } from "./tool-result-normalization.mjs";
import { NormalizerSupervisor } from "./normalizer-supervisor.mjs";
import { normalizeLegacyRaw, normalizeLegacyValue, normalizeLegacyStable } from "./legacy-workload-normalizers.mjs";

const MAX_BODY = 16 * 1024 * 1024;
const MAX_DELAY_MS = 60 * 60 * 1000;
const MAX_REPLAY_ORDER_WAIT_MS = 30000;
const MAX_REPLAY_ORDER_WAITERS = 64;
let stopping = false;
// A replay response must give clients fresh protocol identifiers, yet later
// client history requests legitimately refer to those identifiers.  Keep the
// relationship private to this proxy session so request normalization can
// recover the cassette marker without treating arbitrary opaque user values as
// volatile.  The request and body limits bound this map for a session.
const replayMaterializedIdentifiers = new Map();
let replayMaterializedIdentifierSequence = 0;
const agentWireExtension = process.env.ASB_TAPE_AGENT_WIRE_EXTENSION
  ? await import(pathToFileURL(process.env.ASB_TAPE_AGENT_WIRE_EXTENSION).href)
  : null;
// Adapter-owned normalizers are distinct from wire extensions. They receive
// decoded requests and can change only recognized incidental rendering.
const agentNormalizer = process.argv[2] === "replay" && process.env.ASB_TAPE_AGENT_NORMALIZER
  ? await import(pathToFileURL(process.env.ASB_TAPE_AGENT_NORMALIZER).href)
  : null;
// The parent supplies frozen project policy explicitly; cassette contents are
// never treated as executable modules. Ordinary ASB sessions clear this env.
const customPolicyPath = process.env.ASB_TAPE_CUSTOM_NORMALIZERS;
delete process.env.ASB_TAPE_CUSTOM_NORMALIZERS;
const normalizationEvidencePath = process.env.ASB_TAPE_NORMALIZATION_EVIDENCE;
delete process.env.ASB_TAPE_NORMALIZATION_EVIDENCE;
if (normalizationEvidencePath && !isAbsolute(normalizationEvidencePath)) {
  throw new Error("invalid private normalization evidence destination");
}
const customPolicy = customPolicyPath ? JSON.parse(readFileSync(customPolicyPath, "utf8")) : null;
const readyFile = process.env.ASB_TAPE_READY_FILE;
if (customPolicy && !/^[0-9a-f]{64}$/.test(customPolicy.sha256 || "")) {
  throw new Error("invalid frozen normalizer policy identity");
}
const customSupervisor = customPolicy && process.argv[2] === "replay" ? new NormalizerSupervisor(customPolicy.profiles) : null;
if (customSupervisor) {
  try { await customSupervisor.ready; }
  catch (error) {
    // Worker errors can contain arbitrary data. Publish only a known failure
    // class and a configured profile ID into the private readiness channel.
    const reasons = ["timed out during initialization", "has invalid syntax", "could not be initialized"];
    const match = customPolicy.profiles.flatMap(({ id }) => reasons.map((reason) => ({ id, reason })))
      .find(({ id, reason }) => error?.message === `normalizer ${id}: ${reason}`);
    if (readyFile && isAbsolute(readyFile)) {
      try {
        const temporary = `${readyFile}.tmp`;
        writeFileSync(temporary, JSON.stringify({ schema_version: 1, pid: process.pid,
          error: { kind: "normalizer_startup", profile: match?.id ?? null, reason: match?.reason ?? "worker failed" } }),
          { encoding: "utf8", mode: 0o600, flag: "wx" });
        renameSync(temporary, readyFile);
      } catch { /* normal startup failure remains visible if publication fails */ }
    }
    throw error;
  }
}
let normalizationFailed = false;

function normalizationEvidence(includeChanges = false) {
  if (!customSupervisor) return undefined;
  return {
    policy_sha256: customPolicy.sha256,
    // These are visits to result blocks, including repeated history in later
    // requests. They must not be interpreted as distinct tool executions.
    count_scope: "result-block-visits-including-repeated-history",
    failed: normalizationFailed,
    profiles: customSupervisor.diagnostics(),
    ...(includeChanges ? { changed_result_digests: customSupervisor.changedResultDigests() } : {}),
  };
}

async function normalizeCustomToolResults(body, url) {
  if (!customSupervisor || !body) return body;
  let parsed;
  try { parsed = typeof body === "string" ? JSON.parse(body) : body; }
  catch { return body; }
  try {
    const associationBody = structuredClone(parsed);
    const extension = agentWireExtension?.normalizeRequest?.({ url, body: parsed });
    const envelope = extension === undefined ? parsed : extension;
    if (!envelope || typeof envelope !== "object" || Array.isArray(envelope)) {
      throw new Error("adapter wire extension must return an object or undefined");
    }
    const agent = agentNormalizer?.normalizeRequest?.({ url, body: envelope, profileActive: true });
    if (agent !== undefined && (!agent || typeof agent !== "object" || Array.isArray(agent))) {
      throw new Error("adapter normalizer must return an object or undefined");
    }
    const normalized = await normalizeToolResultsAsync(agent === undefined ? envelope : agent, (context) =>
      customSupervisor.normalize({ ...context, workload: customPolicy.workload }), { associationBody });
    return JSON.stringify(normalized);
  } catch (error) {
    normalizationFailed = true;
    await customSupervisor.close();
    throw error;
  }
}

function usage(message) {
  if (message) console.error(`llmtape: ${message}`);
  console.error("usage: llmtape record --port PORT --upstream URL --out FILE");
  console.error("       llmtape replay --port PORT --cassette FILE");
  process.exit(2);
}

function option(args, name, required = true) {
  const index = args.indexOf(name);
  if (index < 0) {
    if (required) usage(`missing ${name}`);
    return undefined;
  }
  const value = args[index + 1];
  if (!value || value.startsWith("--")) usage(`${name} requires a value`);
  return value;
}

const args = process.argv.slice(2);
const mode = args.shift();
// Older recordings deliberately omitted Codex's parallel-call setting from
// matching. Preserve their historical replay behavior; new recordings retain
// that field and compare it strictly.
let legacyRequestPolicy = false;
if (mode !== "record" && mode !== "replay") usage("mode must be record or replay");
const port = Number(option(args, "--port"));
if (!Number.isInteger(port) || port < (readyFile ? 0 : 1) || port > 65535) usage("--port must be a valid TCP port");
const cassette = mode === "record" ? option(args, "--out") : option(args, "--cassette");
const upstream = mode === "record" ? option(args, "--upstream") : undefined;
const mismatchEvidence = process.env.ASB_TAPE_MISMATCH_EVIDENCE;
const respectRecordTimings = args.includes("--respect-record-timings");
const captureReceipt = process.env.ASB_TAPE_CAPTURE_RECEIPT;
const captureNonce = process.env.ASB_TAPE_CAPTURE_NONCE;
delete process.env.ASB_TAPE_CAPTURE_RECEIPT;
delete process.env.ASB_TAPE_CAPTURE_NONCE;
if (Boolean(captureReceipt) !== Boolean(captureNonce)
  || (captureReceipt && (mode !== "record" || captureNonce.length > 128 || !isAbsolute(cassette)))) {
  usage("invalid private capture invocation");
}
const capture = captureReceipt
  ? new CaptureSession(cassette, { pid: process.pid, nonce: captureNonce })
  : null;
const activeCaptures = new Set();
const requestTracing = new RequestTracing();
if (respectRecordTimings && mode !== "replay") {
  usage("--respect-record-timings is only valid with replay");
}
if (mismatchEvidence && (mode !== "replay" || !isAbsolute(mismatchEvidence))) {
  usage("invalid private mismatch evidence destination");
}

function headersObject(headers) {
  const result = {};
  for (const [name, value] of headers) result[name] = value;
  return result;
}

function requestPath(raw) {
  try {
    const parsed = new URL(raw || "/", "http://asb-tape.invalid");
    return `${parsed.pathname}${parsed.search}`;
  } catch {
    return raw || "/";
  }
}

function redactHeaders(headers) {
  return Object.fromEntries(
    Object.entries(headers).map(([name, value]) =>
      /key|auth|token|secret|credential/i.test(name) ? [name, "[redacted]"] : [name, value],
    ),
  );
}

function responseHeaders(headers) {
  const result = {};
  for (const [name, value] of headers) {
    // `fetch` exposes a decoded body, including when its upstream response was
    // compressed.  Forwarding the original content-encoding would make
    // OpenAI-compatible clients attempt to decode the already-decoded body and
    // surface a misleading connection failure.  These are hop/body framing
    // fields, so the fresh loopback response must let Node compute them.
    if (!/^(connection|content-encoding|content-length|transfer-encoding)$/i.test(name)) {
      result[name] = value;
    }
  }
  return result;
}

// A forwarded streamed provider reply is conclusive only when the upstream
// body has a terminal SSE frame and this proxy has called `response.end`.
// `handle` reaches this point only after its async request-body iterator
// completed, which is the reliable clean-EOF/body-complete proof when this
// client immediately closes HTTP/1.1. Node's later `aborted` flag is not: it
// is set for that normal immediate-close pattern. This never promotes a
// partial stream or arbitrary close to a completed capture.

// Preserve server-sent event boundaries during replay.  Several agent CLIs
// dispatch tool calls as soon as the terminating SSE event arrives; sending
// the complete cassette body as one write can make those clients finish the
// turn before their tool dispatcher observes the event sequence.
async function replayResponse(response, interaction) {
  // Provider responses can contain agent tool-call arguments with the
  // recording workspace path. Materialize the current run's canonical path
  // before replay so agents never execute commands in the record directory.
  const workspace = process.env.ASB_TAPE_WORKSPACE;
  const originalBody = workspace && interaction.asb_request_policy === 3 && interaction.recorded_workspace
    ? normalizeRecordedResponse(interaction.response.body || "", interaction.recorded_workspace)
    : interaction.response.body || "";
  const body = workspace
    ? replaceExactWorkspace(normalizeOpenAiSseWorkspacePaths(normalizeSseWorkspaceFragments(
        originalBody,
        "<asb-workspace>",
        workspace,
        true,
      ), "<asb-workspace>", workspace, true), "<asb-workspace>", workspace)
    : originalBody;
  // Request matching intentionally redacts provider-generated tool-call IDs,
  // but Claude requires IDs to be unique across turns. Give each replayed
  // response a fresh local ID without changing the cassette evidence.
  const replayToolIds = new Map();
  let replayBody = body.replace(/<asb-(?:call(?:-\d+)?|tool-call-\d+|content-block-\d+)>/g, (marker) => {
    if (!replayToolIds.has(marker)) {
      const identifier = `asb-replay-${process.pid}-${Date.now()}-${++replayMaterializedIdentifierSequence}`;
      replayToolIds.set(marker, identifier);
      replayMaterializedIdentifiers.set(identifier, marker);
    }
    return replayToolIds.get(marker);
  });
  replayBody = agentWireExtension?.rewriteReplayResponse?.({body: replayBody, interaction}) ?? replayBody;
  const contentType = Object.entries(interaction.response.headers || {})
    .find(([name]) => name.toLowerCase() === "content-type")?.[1] || "";
  response.writeHead(interaction.response.status, interaction.response.headers || {});
  if (!/text\/event-stream/i.test(contentType) || !replayBody.includes("\n\n")) {
    response.end(replayBody);
    return;
  }
  const events = replayBody.match(/[\s\S]*?(?:\r?\n\r?\n|$)/g)?.filter(Boolean) || [replayBody];
  for (const event of events) {
    response.write(event);
    await new Promise((resolve) => setImmediate(resolve));
  }
  response.end();
}

async function deliverRecordedResponse(response, interaction) {
  const delayMs = Number(interaction.delay_ms);
  if (respectRecordTimings && Number.isFinite(delayMs) && delayMs > 0) {
    await new Promise((resolve) => setTimeout(resolve, Math.min(delayMs, MAX_DELAY_MS)));
  }
  await replayResponse(response, interaction);
}

// Some agent CLIs emit Anthropic server-tool descriptors without the
// input_schema required by the Anthropic wire format.  The loopback bridge is
// deliberately provider-neutral, so make that descriptor valid while keeping
// the request deterministic for recording and replay.  This does not invent
// tool calls; it only supplies the empty schema required for validation.
function normalizeRequestBody(url, body) {
  if (!body || (!url.includes("/messages") && !url.includes("/responses") && !url.includes("/chat/completions"))) return body;
  try {
    const parsed = JSON.parse(body);
    let changed = false;
    // Codex 0.162 emits an internal Responses input item named
    // `additional_tools`. The local bridge intentionally does not expose that
    // internal server-tool type. It is optional metadata, so omit it at the
    // tape boundary while retaining the actual agent tools and prompt. The
    // normalized body is used for both forwarding and cassette keys, making
    // record/replay deterministic.
    if (url.includes("/responses") && Array.isArray(parsed.input)) {
      const filtered = parsed.input.filter((item) => item?.type !== "additional_tools");
      if (filtered.length !== parsed.input.length) {
        parsed.input = filtered;
        changed = true;
      }
    }
    if (url.includes("/responses") && parsed.reasoning && typeof parsed.reasoning === "object") {
      // The bridge accepts only effort and summary. Newer Codex clients add a
      // local `context` field; it is not provider input and must not make a
      // recorded request fail at the compatibility boundary.
      if (Object.hasOwn(parsed.reasoning, "context")) {
        delete parsed.reasoning.context;
        changed = true;
      }
    }
    if (url.includes("/messages") && parsed.temperature === undefined) {
      // Keep record/replay agent trajectories stable when a CLI leaves the
      // provider's sampling default unspecified.
      parsed.temperature = 0;
      changed = true;
    }
    if (!Array.isArray(parsed.tools)) return changed ? JSON.stringify(parsed) : body;
    parsed.tools = parsed.tools.map((tool) => {
      if (tool && tool.type === "web_search_20250305" && !tool.input_schema) {
        changed = true;
        return { ...tool, input_schema: { type: "object", properties: {} } };
      }
      return tool;
      });
    return changed ? JSON.stringify(parsed) : body;
  } catch {
    return body;
  }
}


// Preserve provider response content exactly, apart from workspace paths that
// must point into the replay replica. Request canonicalization is only for
// matching and must never rewrite dates, UUIDs or other model-generated data.
function normalizeRecordedResponse(body, workspace) {
  const marker = "<asb-workspace>";
  // JSON may escape slashes, so rewrite decoded string values before
  // serializing rather than relying on bytewise path matching.
  try {
    JSON.parse(body); // Validate, but leave numbers and untouched tokens byte-for-byte intact.
    return body.replace(/"(?:\\.|[^"\\])*"/g, (token) => {
      const decoded = JSON.parse(token);
      const rewritten = replaceExactWorkspace(decoded, workspace, marker);
      return rewritten === decoded ? token : JSON.stringify(rewritten);
    });
  } catch {
    // SSE and non-JSON response bodies are handled as text below.
  }
  const fragmented = normalizeSseWorkspaceFragments(body, workspace, marker, true);
  return replaceExactWorkspace(
    normalizeOpenAiSseWorkspacePaths(fragmented, workspace, marker, true), workspace, marker,
  );
}

function normalizeCassetteBody(body, url = "", recordedWorkspace = process.env.ASB_TAPE_WORKSPACE) {
  if (!body) return body;
  if (typeof body !== "string") body = JSON.stringify(body);
  const workspace = recordedWorkspace;
  const normalized = customSupervisor ? body : normalizeLegacyRaw(body, workspace, normalizeSseWorkspaceFragments);
  let normalizerInvoked = false;
  try {
    const parsed = JSON.parse(normalized);
    // The tape core passes opaque request data to an optional adapter-owned
    // extension. It neither identifies the adapter nor interprets its fields.
    // Profile-enabled requests already had the agent envelope applied before
    // tool-result normalization. Saved policy requests are canonical and
    // must not have that transformation run again during replay matching.
    const extensionValue = customSupervisor ? undefined : agentWireExtension?.normalizeRequest?.({ url, body: parsed });
    const extensionCanonical = extensionValue === undefined ? parsed : extensionValue;
    normalizerInvoked = Boolean(agentNormalizer?.normalizeRequest);
    const normalizerValue = customSupervisor ? undefined : agentNormalizer?.normalizeRequest?.({ url, body: extensionCanonical });
    if (normalizerValue !== undefined && (!normalizerValue || typeof normalizerValue !== "object" || Array.isArray(normalizerValue))) {
      throw new Error("adapter normalizer must return an object or undefined");
    }
    let canonical = normalizerValue === undefined ? extensionCanonical : normalizerValue;
    const comparisonValue = agentNormalizer?.normalizeComparison?.({ url, body: canonical });
    if (comparisonValue !== undefined && (!comparisonValue || typeof comparisonValue !== "object" || Array.isArray(comparisonValue))) {
      throw new Error("adapter comparison normalizer must return an object or undefined");
    }
    if (comparisonValue !== undefined) canonical = comparisonValue;
    if (legacyRequestPolicy && url.includes('/responses') && canonical?.client_metadata &&
        Object.hasOwn(canonical.client_metadata, 'x-codex-installation-id')) {
      delete canonical.parallel_tool_calls;
    }
    const structuredToolCallIdentifiers = new Map();
    const structuredContentBlockIdentifiers = new Map();
    const canonicalToolCallIdentifier = (identifier) => {
      const materialized = replayMaterializedIdentifiers.get(identifier);
      // A replay response has already assigned this exact identifier to a
      // cassette tool-call marker.  Retain that relationship instead of
      // re-numbering it by whichever history fields this client happens to
      // retain on its next request.
      if (/^<asb-tool-call-\d+>$/.test(materialized || "")) return materialized;
      identifier = materialized || identifier;
      if (!structuredToolCallIdentifiers.has(identifier)) {
        structuredToolCallIdentifiers.set(
          identifier,
          `<asb-tool-call-${structuredToolCallIdentifiers.size + 1}>`,
        );
      }
      return structuredToolCallIdentifiers.get(identifier);
    };
    const canonicalContentBlockIdentifier = (identifier) => {
      const materialized = replayMaterializedIdentifiers.get(identifier);
      if (/^<asb-content-block-\d+>$/.test(materialized || "")) return materialized;
      identifier = materialized || identifier;
      if (!structuredContentBlockIdentifiers.has(identifier)) {
        structuredContentBlockIdentifiers.set(
          identifier,
          `<asb-content-block-${structuredContentBlockIdentifiers.size + 1}>`,
        );
      }
      return structuredContentBlockIdentifiers.get(identifier);
    };
    const sort = (value, key = "", path = []) => {
      // OpenAI-compatible agents can generate a fresh opaque identifier for
      // an otherwise identical tool turn.  Normalize only the protocol's
      // explicit tool-call identity fields, retaining their relationship
      // within this request.  Do not touch similarly named prompt data or
      // function arguments, which remain replay-significant.
      if (typeof value === "string"
          && path.length === 5
          && path[0] === "messages"
          && /^\d+$/.test(path[1])
          && path[2] === "tool_calls"
          && /^\d+$/.test(path[3])
          && path[4] === "id"
          && key === "id") {
        return canonicalToolCallIdentifier(value);
      }
      if (typeof value === "string"
          && path.length === 5
          && path[0] === "messages"
          && /^\d+$/.test(path[1])
          && path[2] === "content"
          && /^\d+$/.test(path[3])
          && path[4] === "tool_use_id"
          && key === "tool_use_id") {
        return canonicalContentBlockIdentifier(value);
      }
      if (typeof value === "string"
          && path.length === 3
          && path[0] === "messages"
          && /^\d+$/.test(path[1])
          && path[2] === "tool_call_id"
          && key === "tool_call_id") {
        return canonicalToolCallIdentifier(value);
      }
      // Anthropic-compatible clients assign a fresh opaque ID to each
      // assistant content block.  It identifies the provider protocol turn,
      // not user input or a tool argument; canonicalize the fixed wire path
      // while retaining all neighboring block fields for strict matching.
      if (typeof value === "string"
          && path.length === 5
          && path[0] === "messages"
          && /^\d+$/.test(path[1])
          && path[2] === "content"
          && /^\d+$/.test(path[3])
          && path[4] === "id"
          && key === "id") {
        return canonicalContentBlockIdentifier(value);
      }
      if (typeof value === "string") return customSupervisor ? replaceExactWorkspace(value, workspace, "<asb-workspace>") : normalizeLegacyValue(value);
      if (Array.isArray(value)) return value.map((item, index) => sort(item, String(index), [...path, String(index)]));
      if (value && typeof value === "object") {
        return Object.fromEntries(Object.keys(value).sort().map((child) => [child, sort(value[child], child, [...path, child])]));
      }
      return value;
    };
    const stable = JSON.stringify(sort(canonical));
    return customSupervisor ? stable : normalizeLegacyStable(stable);
  } catch (error) {
    // An adapter-selected script is part of the matching contract.  Never
    // silently fall back to a different key if it rejected its bounded input
    // or returned malformed data; doing so would hide an unsafe replay drift.
    if (normalizerInvoked) throw error;
    return normalized;
  }
}

// Tool-call arguments in Anthropic SSE responses are split across many
// `input_json_delta` events. A workspace path can therefore be divided across
// event boundaries and cannot be normalized by a regex over the raw SSE text.
// Reassemble each contiguous content block, normalize it, and emit the result
// in the first fragment while retaining the event boundaries for the client.
function normalizeSseWorkspaceFragments(body, workspace, replacement = "<asb-workspace>", exactOnly = false) {
  if (!body.includes("partial_json")) return body;
  const lines = body.split(/(?<=\n)/);
  const groups = [];
  const active = new Map();
  for (let index = 0; index < lines.length; index += 1) {
    const match = lines[index].match(/^(data:\s+)(\{.*\})(\r?\n)?$/);
    if (!match) continue;
    let event;
    try { event = JSON.parse(match[2]); } catch { continue; }
    const partial = event?.delta?.partial_json;
    const block = event?.index;
    if (typeof partial === "string" && Number.isInteger(block)) {
      const group = active.get(block) || { indices: [], parts: [] };
      group.indices.push(index);
      group.parts.push(partial);
      active.set(block, group);
    }
    if (event?.type === "content_block_stop" && Number.isInteger(block)) {
      const group = active.get(block);
      if (group) { groups.push(group); active.delete(block); }
    }
  }
  for (const group of groups) {
    const value = normalizeWorkspaceText(group.parts.join(""), workspace, replacement, exactOnly);
    let cursor = 0;
    for (let offset = 0; offset < group.indices.length; offset += 1) {
      const lineIndex = group.indices[offset];
      const match = lines[lineIndex].match(/^(data:\s+)(\{.*\})(\r?\n)?$/);
      if (!match) continue;
      let event;
      try { event = JSON.parse(match[2]); } catch { continue; }
      const length = group.parts[offset].length;
      const end = offset === group.indices.length - 1 ? value.length : Math.min(value.length, cursor + length);
      event.delta.partial_json = value.slice(cursor, end);
      cursor = end;
      lines[lineIndex] = `${match[1]}${JSON.stringify(event)}${match[3] || ""}`;
    }
  }
  return lines.join("");
}

function normalizeWorkspaceText(value, workspace, replacement = "<asb-workspace>", exactOnly = false) {
  const exact = replaceExactWorkspace(value, workspace, replacement);
  return exactOnly ? exact : normalizeWorkspacePaths(exact, replacement);
}

function requestKey(request) {
  // Match the normalized full request, not just model and endpoint: a changed
  // prompt or tool state must not silently consume an unrelated response.
  return JSON.stringify([request.method, request.url, normalizeCassetteBody(request.body, request.url)]);
}

const rawInteractions = mode === "replay"
  ? readFileSync(cassette, "utf8")
      .split(/\r?\n/)
      .filter(Boolean)
      .map((line) => JSON.parse(line))
  : [];
legacyRequestPolicy = mode === 'replay' && rawInteractions.length > 0 &&
  rawInteractions.every(interaction => interaction.asb_request_policy === undefined);
if (mode === 'replay') agentWireExtension?.configureReplay?.({interactions:rawInteractions});
if (mode === 'replay' && !legacyRequestPolicy && (rawInteractions.some(interaction => ![2, 3].includes(interaction.asb_request_policy)) || new Set(rawInteractions.map(interaction => interaction.asb_request_policy)).size !== 1)) {
  throw new Error('cassette request policy is inconsistent');
}
const interactions = [];
for (const interaction of rawInteractions) {
    if (interaction.request) {
      interaction.request.url = requestPath(interaction.request.url);
      // Recordings already contain the policy's comparison copy. Never apply
      // a policy twice or silently apply one to a legacy unmarked recording.
      const recordedPolicy = interaction.request.custom_normalization_sha256;
      if (recordedPolicy !== undefined && (!customPolicy || recordedPolicy !== customPolicy.sha256)) {
        throw new Error("recorded normalizer policy differs from the selected policy; record a new cassette");
      }
      if (customPolicy && recordedPolicy === undefined && interaction.request.body && interaction.asb_request_policy !== 3) {
        throw new Error("recording has no normalizer policy identity; record a new cassette");
      }
      if (interaction.asb_request_policy === 3 && interaction.request.body) {
        const body = normalizeRequestBody(interaction.request.url, interaction.request.body);
        interaction.request.body = normalizeCassetteBody(await normalizeCustomToolResults(body, interaction.request.url), interaction.request.url, interaction.recorded_workspace);
      } else {
        interaction.request.body = normalizeCassetteBody(interaction.request.body, interaction.request.url);
      }
    }
    interactions.push(interaction);
}
// Recorded and live requests must build independent session aliases in the
// same encounter order. Preparing recorded keys must not seed the live side.
agentWireExtension?.resetReplayComparison?.();
let cursor = 0;
let replayOrderWaiters = 0;
// Optional probes advance the recorded cursor when omitted. Keep their exact
// entries available for a delayed matching probe, at most once per entry.
const deferredProbes = [];
const skippedProbeIndices = new Set();
// Nested provider exchanges belong to a particular recorded native tool.
// The tool adapter signals only after matching that exact tool invocation.
const completedNativeTools = new Set();
function nativeToolKey(context) {
  if (!context || typeof context.agent !== 'string' || !/^[a-z][a-z0-9-]{0,31}$/.test(context.agent) || !Number.isSafeInteger(context.ordinal) || context.ordinal < 0) return null;
  return `${context.agent}:${context.ordinal}`;
}
function advanceNativeTools() {
  while (cursor < interactions.length) {
    const key = nativeToolKey(interactions[cursor]?.native_tool_context);
    if (!key || !completedNativeTools.has(key)) break;
    skippedProbeIndices.add(cursor);
    cursor += 1;
  }
}
let rejected = 0;
let firstMismatch = null;
const progressPath = `${cassette}.progress.json`;
function writeProgress() {
  if (mode === "replay") {
    writeFileSync(progressPath, JSON.stringify({
      schema_version: 2,
      consumed: cursor,
      total: interactions.length,
      rejected,
      unrequested_optional_probes: deferredProbes.length,
      ...(customSupervisor ? { normalization: normalizationEvidence() } : {}),
      ...(firstMismatch ? { first_mismatch: firstMismatch } : {}),
    }));
  }
}

function safePathSegment(key) {
  // Diagnostic paths are deliberately structural: never write request values
  // or arbitrary/sensitive object keys into progress evidence.
  if (/token|secret|password|authorization|credential|api[_-]?key/i.test(key)) {
    return ".<redacted>";
  }
  return /^[a-z][a-z0-9_]*$/i.test(key) && key.length <= 48
    ? `.${key}`
    : ".<field>";
}

function stringFeatures(value) {
  return {
    lines: value.split("\n").length,
    has_agents_guidance: value.includes("AGENTS.md"),
    has_parent_path: value.includes("../"),
    has_workspace_path: value.includes("<asb-workspace>") || /workspace(?:-concurrency-\d+)?/.test(value),
    has_additional_input: /additional input/i.test(value),
  };
}

function jsonDifference(expected, actual, path = "$") {
  if (expected === actual) return null;
  const expectedArray = Array.isArray(expected);
  const actualArray = Array.isArray(actual);
  if (expectedArray !== actualArray || typeof expected !== typeof actual || expected === null || actual === null) {
    return { category: "json_type", path };
  }
  if (expectedArray) {
    if (expected.length !== actual.length) return { category: "array_length", path };
    for (let index = 0; index < expected.length; index += 1) {
      const difference = jsonDifference(expected[index], actual[index], `${path}[${index}]`);
      if (difference) return difference;
    }
    return null;
  }
  if (typeof expected === "object") {
    const expectedKeys = Object.keys(expected).sort();
    const actualKeys = Object.keys(actual).sort();
    if (expectedKeys.length !== actualKeys.length || expectedKeys.some((key, index) => key !== actualKeys[index])) {
      return { category: "object_keys", path };
    }
    for (const key of expectedKeys) {
      const difference = jsonDifference(expected[key], actual[key], `${path}${safePathSegment(key)}`);
      if (difference) return difference;
    }
    return null;
  }
  if (expected === actual) return null;
  if (typeof expected === "string" && typeof actual === "string") {
    let offset = 0;
    while (offset < expected.length && offset < actual.length && expected[offset] === actual[offset]) {
      offset += 1;
    }
    // Values remain private: lengths and the first differing offset are
    // sufficient to distinguish generated suffixes from a changed prompt.
    return {
      category: "json_value",
      path,
      string_lengths: { expected: expected.length, actual: actual.length },
      first_difference_offset: offset,
      string_features: { expected: stringFeatures(expected), actual: stringFeatures(actual) },
    };
  }
  return { category: "json_value", path };
}

function requestMismatch(expected, actual, sequence) {
  const mismatch = { sequence, category: "request", path: "$" };
  if (expected.method !== actual.method) return { ...mismatch, category: "method" };
  if (expected.url !== actual.url) return { ...mismatch, category: "url" };
  try {
    const difference = jsonDifference(JSON.parse(expected.body ?? "null"), JSON.parse(actual.body ?? "null"));
    return difference ? { ...mismatch, ...difference } : mismatch;
  } catch {
    return { ...mismatch, category: "body" };
  }
}

function redactMismatchValue(value, key = "") {
  // Numeric generation limits are not credentials and are useful when a
  // replay differs only in the model's configured output budget.
  if (["max_tokens", "max_completion_tokens"].includes(key) && Number.isSafeInteger(value) && value >= 0) {
    return value;
  }
  if (/token|secret|password|authorization|credential|api[_-]?key/i.test(key)) {
    return "[redacted]";
  }
  if (Array.isArray(value)) return value.map((item) => redactMismatchValue(item));
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([child, item]) => [child, redactMismatchValue(item, child)]));
  }
  return value;
}

function diagnosticRequest(request) {
  const body = request.body ?? "";
  try {
    return { method: request.method, url: request.url, body: redactMismatchValue(JSON.parse(body)) };
  } catch {
    const redacted = body.replace(
      /((?:token|secret|password|authorization|credential|api[_-]?key)\s*[=:]\s*)([^\s,;]+)/gi,
      "$1[redacted]",
    );
    return { method: request.method, url: request.url, body: redacted.length <= MAX_BODY ? redacted : "[omitted: body exceeds limit]" };
  }
}

function retainMismatchEvidence(expected, actual, mismatch) {
  if (!mismatchEvidence || existsSync(mismatchEvidence)) return;
  const evidence = JSON.stringify({
    schema_version: 1,
    mismatch,
    ...(customSupervisor ? { normalization: normalizationEvidence(true) } : {}),
    expected: diagnosticRequest(expected),
    actual: diagnosticRequest(actual),
  });
  if (Buffer.byteLength(evidence, "utf8") > 2 * MAX_BODY) return;
  // A diagnostic artifact is best-effort: inability to retain it must never
  // convert the original strict replay mismatch into a misleading transport
  // failure.
  try {
    writeFileSync(mismatchEvidence, evidence, { encoding: "utf8", mode: 0o600, flag: "wx" });
  } catch {}
}

function reject(mismatch) {
  rejected += 1;
  firstMismatch ??= mismatch;
  writeProgress();
}

function isReplayProbe(request) {
  if (agentWireExtension?.isReplayProbe?.(request) === true) return true;
  if (!request.body) {
    return (request.method === "HEAD" || request.method === "GET")
      && /(?:\/api\/hello|\/health(?:z)?|\/models|\/api\/tags)(?:[/?]|$)/.test(request.url || "");
  }
  // A request with a body may be a real inference or embedding operation,
  // even when its model or prompt resembles a capability check. It must be
  // matched and consumed as an ordinary recorded exchange.
  return false;
}

// Adapter-owned probe classes may require a particular recorded response to
// be safely omitted. An absent veto preserves existing probe behavior.
function isSkippableRecordedProbe(interaction) {
  return Boolean(interaction?.request && isReplayProbe(interaction.request)) &&
    agentWireExtension?.maySkipRecordedProbe?.(interaction) !== false;
}

// Replay admission and inference accounting are distinct: embeddings must be
// replayed, but they are not language-model inference requests.
function isInferenceRequest(request) {
  const path = String(request.url || "").split("?", 1)[0].replace(/\/+$/, "");
  return !isReplayProbe(request) && !path.endsWith("/embeddings");
}

// A later exact probe may pass over omitted probes of other classes; an
// unrecorded probe must not consume a different class by accident.
function maySkipCurrentProbe(request) {
  if (!isReplayProbe(request)) return true;
  const key = requestKey(request);
  for (let index = cursor + 1; index < interactions.length; index += 1) {
    if (interactions[index]?.request && requestKey(interactions[index].request) === key) return true;
  }
  return false;
}
writeProgress();

// Requests arriving out of order can be concurrent even when their recorded
// sequence is fixed. Only defer an exact later request; unrelated mismatches
// keep their normal immediate failure. Bound both waiting time and population.
async function awaitReplayTurn(request, response, deadline) {
  if (!interactions[cursor]?.request) return true;
  const key = requestKey(request);
  if (key === requestKey(interactions[cursor].request)) return true;
  let target = -1;
  for (let index = cursor + 1; index < interactions.length; index += 1) {
    if (interactions[index]?.request && requestKey(interactions[index].request) === key) {
      target = index;
      break;
    }
  }
  if (target < 0 || replayOrderWaiters >= MAX_REPLAY_ORDER_WAITERS) return true;
  replayOrderWaiters += 1;
  try {
    while (cursor < target && performance.now() < deadline && !response.destroyed && !stopping) {
      if (isSkippableRecordedProbe(interactions[cursor]) && maySkipCurrentProbe(request)) return true;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    return !response.destroyed && !stopping;
  } finally {
    replayOrderWaiters -= 1;
  }
}

async function handle(request, response, recording) {
  const startedAt = performance.now();
  // Provider-boundary timestamps are authoritative tool-call evidence. ASB
  // never derives tool calls or timings from agent stdout/stderr.
  const requestReceivedUnixMs = Math.round(performance.timeOrigin + startedAt);
  const chunks = [];
  let bodySize = 0;
  for await (const chunk of request.iterator({ destroyOnReturn: false })) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    bodySize += bytes.length;
    if (bodySize > MAX_BODY) {
      if (mode === "replay") {
        reject({ sequence: cursor + 1, category: "body_size_limit", path: "$" });
      }
      request.resume();
      response.writeHead(413, { "content-type": "application/json", connection: "close" });
      response.end(JSON.stringify({ error: "request body exceeds 16 MiB" }));
      return;
    }
    chunks.push(bytes);
  }
  const body = Buffer.concat(chunks, bodySize);
  // Private native-tool attribution is carried only to this proxy. Never
  // forward it to providers or include it in request matching material.
  const attribution = request.headers['x-asb-native-tool'];
  delete request.headers['x-asb-native-tool'];
  let nativeToolContext;
  if (attribution !== undefined) {
    const match = typeof attribution === 'string' && /^v1:([a-z][a-z0-9-]{0,31}):(0|[1-9][0-9]*)$/.exec(attribution);
    const ordinal = match ? Number(match[2]) : NaN;
    if (!match || !Number.isSafeInteger(ordinal)) {
      response.writeHead(400, { connection: 'close' });
      response.end();
      return;
    }
    nativeToolContext = { agent: match[1], ordinal };
  }
  if (requestPath(request.url).split('?', 1)[0].endsWith('/__asb_native_tool_consumed')) {
    let key;
    try { key = nativeToolKey(JSON.parse(body.toString('utf8'))); } catch { /* malformed control */ }
    if (mode !== 'replay' || request.method !== 'POST' || !key) {
      response.writeHead(400, { connection: 'close' });
    } else {
      completedNativeTools.add(key);
      advanceNativeTools();
      writeProgress();
      response.writeHead(204, { connection: 'close' });
    }
    response.end();
    return;
  }
  // Adapter-owned control messages are handled locally and never enter the
  // provider transcript. The tape core does not interpret their payload.
  if (agentWireExtension?.controlRequest) {
    try {
      const control = await agentWireExtension.controlRequest({ method: request.method, url: requestPath(request.url), body, mode });
      if (control !== undefined) {
        response.writeHead(control ? 204 : 400, { connection: "close" });
        response.end();
        return;
      }
    } catch {
      response.writeHead(400, { connection: "close" });
      response.end();
      return;
    }
  }
  let agentRecordMetadata;
  if (mode === 'record' && agentWireExtension?.recordMetadata) {
    let parsed;
    try {
      parsed = JSON.parse(body.toString('utf8'));
    } catch {
      // Invalid/non-JSON provider bodies remain the provider's concern.
    }
    if (parsed !== undefined) agentRecordMetadata = agentWireExtension.recordMetadata({url: requestPath(request.url), body: parsed});
  }
  const originalBody = body.toString("utf8");
  const normalizedBody = mode === "replay" ? normalizeRequestBody(request.url || "/", originalBody) : originalBody;
  const recordedRequest = {
    method: request.method || "GET",
    // Preserve the observed target in the cassette. Routing and replay
    // comparisons independently derive a path so absolute-form requests
    // cannot select a different upstream host.
    url: mode === "record" ? (request.url || "/") : requestPath(request.url),
    headers: redactHeaders(Object.fromEntries(Object.entries(request.headers))),
    body: body.length ? (mode === "replay" ? normalizeCassetteBody(await normalizeCustomToolResults(normalizedBody, request.url || "/"), request.url || "/") : originalBody) : null,
  };

  if (mode === "replay") {
    const deadline = performance.now() + MAX_REPLAY_ORDER_WAIT_MS;
    while (true) {
      advanceNativeTools();
      if (isReplayProbe(recordedRequest)) {
        const deferred = deferredProbes.findIndex((index) => requestKey(interactions[index].request) === requestKey(recordedRequest));
        if (deferred >= 0) {
          const [index] = deferredProbes.splice(deferred, 1);
          writeProgress();
          await deliverRecordedResponse(response, interactions[index]);
          return;
        }
      }
      // Capability probes may be issued by the client even when the replayed
      // run would otherwise omit them.  If the cassette has the corresponding
      // known probe at the cursor, consume it and return its recorded response
      // instead of comparing the probe with the first workload request.
      if (isReplayProbe(recordedRequest)
        && interactions[cursor]?.request
        // Probe classes alone are not interchangeable: an embedding probe must
        // never be served to a vision-capability probe (or vice versa).
        && requestKey(recordedRequest) === requestKey(interactions[cursor].request)) {
        const probe = interactions[cursor];
        cursor += 1;
        advanceNativeTools();
        writeProgress();
        await deliverRecordedResponse(response, probe);
        return;
      }
      // A provider bridge may record a health/model probe that the agent omits
      // on replay. Consume only these known, credential-free probes when the
      // next request is not a probe, or it exactly matches a later probe.
      while (interactions[cursor]?.request
        && maySkipCurrentProbe(recordedRequest)
        && isSkippableRecordedProbe(interactions[cursor])
        // If this client did issue the next probe, it must consume its own
        // recorded exchange. Only probes absent from the client request may be
        // skipped.
        && requestKey(recordedRequest) !== requestKey(interactions[cursor].request)) {
        deferredProbes.push(cursor);
        skippedProbeIndices.add(cursor);
        cursor += 1;
        advanceNativeTools();
        writeProgress();
      }
      const expectedCursor = cursor;
      if (!await awaitReplayTurn(recordedRequest, response, deadline)) return;
      // Another request may have advanced to a probe, a duplicate, or the
      // end while this one waited. Reevaluate before choosing a response.
      if (cursor === expectedCursor) break;
    }
    const interaction = interactions[cursor];
    if (!interaction) {
      reject({ sequence: cursor + 1, category: "cassette_exhausted", path: "$" });
      response.writeHead(409, { "content-type": "application/json" });
      response.end(JSON.stringify({ error: "cassette has no more recorded requests" }));
      return;
    }
    if (requestKey(recordedRequest) !== requestKey(interaction.request)) {
      // A client may retry a request after receiving a valid streamed
      // response (for example when it loses the final SSE marker). Replaying
      // the already-consumed exact request is safe and keeps the cursor on
      // the next interaction; any other mismatch remains fail-closed.
      let retry;
      for (let index = cursor - 1; index >= 0; index -= 1) {
        if (!skippedProbeIndices.has(index) && requestKey(recordedRequest) === requestKey(interactions[index].request)) {
          retry = interactions[index];
          break;
        }
      }
      if (retry) {
        await deliverRecordedResponse(response, retry);
        return;
      }
      const mismatch = requestMismatch(interaction.request, recordedRequest, cursor + 1);
      retainMismatchEvidence(interaction.request, recordedRequest, mismatch);
      reject(mismatch);
      response.writeHead(409, { "content-type": "application/json" });
      response.end(JSON.stringify({ error: "request does not match cassette" }));
      return;
    }
    if (agentWireExtension?.observeReplayExchange) {
      let parsed;
      try { parsed = JSON.parse(body.toString('utf8')); } catch { /* Non-JSON bodies remain opaque. */ }
      if (parsed !== undefined) await agentWireExtension.observeReplayExchange({url:requestPath(request.url),body:parsed,interaction});
    }
    cursor += 1;
    advanceNativeTools();
    writeProgress();
    await deliverRecordedResponse(response, interaction);
    return;
  }

  // Preserve provider-specific prefixes (e.g. OpenRouter's /api) while
  // keeping the selected upstream host, even for absolute-form requests.
  const target = new URL(`${upstream.replace(/\/+$/, "")}${requestPath(recordedRequest.url)}`);
  const forwardedHeaders = { ...request.headers };
  delete forwardedHeaders.host;
  delete forwardedHeaders.connection;
  delete forwardedHeaders["content-length"];
  const upstreamResponse = await networkFetch(target, {
    method: recordedRequest.method,
    headers: forwardedHeaders,
    body: body.length ? Buffer.from(normalizedBody) : undefined,
    signal: recording?.controller.signal,
  });
  const responseChunks = [];
  let responseSize = 0;
  if (upstreamResponse.body) {
    for await (const chunk of upstreamResponse.body) {
      responseSize += chunk.byteLength;
      if (responseSize > MAX_BODY) throw new Error("upstream response body exceeds 16 MiB");
      responseChunks.push(chunk);
    }
  }
  const responseBody = Buffer.concat(responseChunks, responseSize).toString("utf8");
  const recordedResponse = {
    status: upstreamResponse.status,
    headers: responseHeaders(upstreamResponse.headers),
    body: responseBody,
  };
  const delayMs = Math.min(Math.max(0, Math.round(performance.now() - startedAt)), MAX_DELAY_MS);
  const interaction = {
    asb_request_policy: 3,
    ...(process.env.ASB_TAPE_WORKSPACE ? { recorded_workspace: process.env.ASB_TAPE_WORKSPACE } : {}),
    request: recordedRequest,
    response: recordedResponse,
    delay_ms: delayMs,
    timing: {
      request_received_unix_ms: requestReceivedUnixMs,
      response_observed_unix_ms: Math.round(performance.timeOrigin + performance.now()),
    },
    ...(nativeToolContext ? { native_tool_context: nativeToolContext } : {}),
    ...(agentRecordMetadata !== undefined ? { agent_record_metadata: agentRecordMetadata } : {}),
  };
  if (capture) capture.append(recording.token, interaction, isInferenceRequest(recordedRequest));
  else {
    if (stopping) return;
    appendFileSync(cassette, `${JSON.stringify(interaction)}\n`);
  }
  response.writeHead(recordedResponse.status, recordedResponse.headers);
  if (recording) recording.completedSseResponse = completedSseResponse(recordedResponse.headers, responseBody);
  if (recording) recording.responseEnded = true;
  response.end(responseBody);
}

const server = http.createServer((request, response) => {
  const observation = requestTracing.begin();
  let observationSettled = false;
  const settleObservation = (reason) => {
    if (observationSettled) return;
    observationSettled = true;
    if (reason) requestTracing.failure(observation, reason);
    requestTracing.end(observation);
  };
  response.once("finish", () => settleObservation());
  response.once("close", () => settleObservation(response.writableEnded ? undefined : "cancelled"));
  const fail = (error) => {
    settleObservation("http_error");
    if (!response.destroyed) {
      if (!response.headersSent) response.writeHead(502, { "content-type": "application/json" });
      response.end(JSON.stringify({ error: String(error.message || error) }));
    }
  };
  if (!capture) {
    handle(request, response).catch(fail);
    return;
  }
  let token;
  try {
    // Preserve only bounded protocol shape for an incomplete capture's
    // private delivery diagnostic. Query strings/headers/bodies are excluded.
    token = capture.begin({
      method: request.method,
      path: requestPath(request.url).split("?", 1)[0],
    });
  }
  catch (error) { fail(error); return; }
  if (!token) {
    response.writeHead(503, { connection: "close" });
    response.end("recording is draining");
    return;
  }
  const recording = {
    token,
    request,
    response,
    controller: new AbortController(),
    completedSseResponse: false,
    responseEnded: false,
  };
  // Subscribe before handle can finish or close a response. Completion requires
  // both full capture and positive delivery, not merely an upstream EOF.
  const delivered = responseDelivery(response, () =>
    recording.completedSseResponse
      && recording.responseEnded,
  );
  activeCaptures.add(recording);
  recording.done = (async () => {
    let delivery = { terminal: "unsettled", delivered: false };
    try {
      await handle(request, response, recording);
      delivery = await delivered;
    } catch (error) {
      fail(error);
    } finally {
      capture.end(token, delivery);
      activeCaptures.delete(recording);
    }
  })();
});

async function waitCaptures(milliseconds) {
  let timer;
  try {
    await Promise.race([
      Promise.all([...activeCaptures].map((recording) => recording.done)),
      new Promise((resolve) => { timer = setTimeout(resolve, milliseconds); }),
    ]);
  } finally { clearTimeout(timer); }
}

async function shutdown() {
  if (stopping) return;
  stopping = true;
  requestTracing.close();
  capture?.drain();
  server.close();
  try {
    if (capture) {
      server.closeIdleConnections();
      await waitCaptures(300);
      for (const recording of activeCaptures) {
        recording.controller.abort();
        recording.request.destroy();
        recording.response.destroy();
      }
      server.closeAllConnections();
      await Promise.all([quiesceNetworkRequests(), waitCaptures(300)]);
      if (activeCaptures.size !== 0) throw new Error("recording handlers did not quiesce");
      capture.finalize(captureReceipt);
      if (!capture.publicationCleanupConfirmed) throw new Error("capture receipt cleanup unconfirmed");
    } else {
      server.closeAllConnections();
      await quiesceNetworkRequests();
    }
    if (normalizationEvidencePath && customSupervisor) {
      writeFileSync(normalizationEvidencePath, JSON.stringify({ schema_version: 1, ...normalizationEvidence(true) }),
        { encoding: "utf8", mode: 0o600, flag: "wx" });
    }
    await customSupervisor?.close();
    process.exit(0);
  } catch {
    try { capture?.close(); } catch { /* sticky uncertainty: never report cleanup success */ }
    process.exit(2);
  }
}

// Only ASB's embedded invocation enables the private lifetime pipe. A closed
// owner, including one killed before Node starts, produces EOF. Standalone
// and explicit legacy overrides keep their existing stdin contract.
if (process.env.ASB_TAPE_OWNER_PIPE === "1") {
  delete process.env.ASB_TAPE_OWNER_PIPE;
  process.stdin.on("end", shutdown);
  process.stdin.on("error", shutdown);
  process.stdin.resume();
}

if (mode === "record" && !capture) {
  if (existsSync(cassette)) writeFileSync(cassette, "");
}
server.listen(port, "127.0.0.1", () => {
  if (readyFile) {
    const temporary = `${readyFile}.tmp`;
    writeFileSync(temporary, JSON.stringify({
      schema_version: 1, pid: process.pid, host: "127.0.0.1", port: server.address().port,
    }), { mode: 0o600, flag: "wx" });
    renameSync(temporary, readyFile);
  }
});
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, shutdown);
}
