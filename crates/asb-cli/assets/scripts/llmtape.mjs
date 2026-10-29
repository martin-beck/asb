#!/usr/bin/env node

// Small project-local HTTP recorder/replayer embedded in ASB. It supplies the
// bounded loopback protocol that ASB's tape module invokes.

import { appendFileSync, existsSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import http from "node:http";
import { performance } from "node:perf_hooks";
import { isAbsolute } from "node:path";
import { quiesceNetworkRequests, networkFetch } from "./network-fetch.mjs";
import { CaptureSession, responseDelivery } from "./capture-session.mjs";
import { normalizeWorkspacePaths } from "./tape-normalization.mjs";

const MAX_BODY = 16 * 1024 * 1024;
const MAX_DELAY_MS = 60 * 60 * 1000;
let stopping = false;
// A replay response must give clients fresh protocol identifiers, yet later
// client history requests legitimately refer to those identifiers.  Keep the
// relationship private to this proxy session so request normalization can
// recover the cassette marker without treating arbitrary opaque user values as
// volatile.  The request and body limits bound this map for a session.
const replayMaterializedIdentifiers = new Map();
let replayMaterializedIdentifierSequence = 0;

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
if (mode !== "record" && mode !== "replay") usage("mode must be record or replay");
const port = Number(option(args, "--port"));
const readyFile = process.env.ASB_TAPE_READY_FILE;
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

// Preserve server-sent event boundaries during replay.  Several agent CLIs
// dispatch tool calls as soon as the terminating SSE event arrives; sending
// the complete cassette body as one write can make those clients finish the
// turn before their tool dispatcher observes the event sequence.
async function replayResponse(response, interaction) {
  // Provider responses can contain agent tool-call arguments with the
  // recording workspace path. Materialize the current run's canonical path
  // before replay so agents never execute commands in the record directory.
  const workspace = process.env.ASB_TAPE_WORKSPACE;
  const body = workspace
    ? normalizeSseWorkspaceFragments(
        normalizeCassetteBody(interaction.response.body || ""),
        "<asb-workspace>",
        workspace,
      ).replaceAll("<asb-workspace>", workspace)
    : interaction.response.body || "";
  // Request matching intentionally redacts provider-generated tool-call IDs,
  // but Claude requires IDs to be unique across turns. Give each replayed
  // response a fresh local ID without changing the cassette evidence.
  const replayToolIds = new Map();
  const replayBody = body.replace(/<asb-(?:call(?:-\d+)?|tool-call-\d+|content-block-\d+)>/g, (marker) => {
    if (!replayToolIds.has(marker)) {
      const identifier = `asb-replay-${process.pid}-${Date.now()}-${++replayMaterializedIdentifierSequence}`;
      replayToolIds.set(marker, identifier);
      replayMaterializedIdentifiers.set(identifier, marker);
    }
    return replayToolIds.get(marker);
  });
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
    parsed.tools = parsed.tools
      .filter((tool) => {
        // The local bridge supports function tools.  Cline's Responses
        // adapter may add native web/image tools that the bridge cannot
        // execute; omit those optional tools while retaining agent tools.
        if (url.includes("/responses") && tool &&
            (tool.type === "web_search" || tool.type === "image_generation")) {
          changed = true;
          return false;
        }
        return true;
      })
      .map((tool) => {
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

// Result workspaces are intentionally different for record and replay.  Keep
// absolute project paths out of cassette keys while forwarding the normalized
// request body unchanged to the live provider during recording.
function normalizeToolSearchOrdering(value) {
  // OpenCode's built-in repository search can return identical match blocks in
  // filesystem enumeration order. Fresh workload materialization is allowed
  // to change that order, but not the observed paths or lines. Restrict the
  // canonicalization to its fully formed /testbed match report so prompts,
  // commands, and arbitrary tool output remain cassette-significant.
  const header = value.match(/^(Found \d+ matches\n)/);
  if (!header) return value;
  const body = value.slice(header[0].length);
  const trailingNewline = body.endsWith("\n");
  const blocks = body.split(/\n{2,}(?=\/testbed\/)/).map((block) => block.replace(/\n+$/, ""));
  if (blocks.length < 2 || blocks.some((block) => !block.startsWith("/testbed/"))) {
    return value;
  }
  return header[0] + blocks.sort((left, right) => left.localeCompare(right)).join("\n\n")
    + (trailingNewline ? "\n" : "");
}

function normalizeLsDiagnosticOrdering(value) {
  // A single `ls` invocation can write directory entries to stdout while a
  // missing operand diagnostic goes to stderr. OpenCode captures both streams
  // in one tool result, but their relative scheduling is not stable. Retain
  // every diagnostic and every listing line; only place these fixed `ls`
  // diagnostics before the corresponding stdout rendering for comparison.
  const lines = value.split("\n");
  const diagnostics = lines.filter((line) => /^ls: cannot access '.+': /.test(line));
  if (diagnostics.length === 0) return value;
  const ordinary = lines.filter((line) => !/^ls: cannot access '.+': /.test(line));
  return [...diagnostics.sort(), ...ordinary].join("\n");
}

function normalizePytestElapsed(value) {
  // JSON encodes an earlier line feed as the two characters `\\n`, so a
  // word-boundary-only matcher misses a summary in a structured tool value.
  // Retain every count/category; only elapsed rendering is volatile.
  return value.replace(/(^|\\n|\r?\n)((?:\d+ (?:failed|passed|skipped|deselected|errors?),? )+in )\d+(?:\.\d+)?s\b/g, "$1$2<asb-elapsed>");
}

// Python renders a callable's process-local address in its fixed repr.  This
// is terminal presentation noise, not a tool result change.  Keep the exact
// existing callable-rendering rule in one helper so it applies both before
// parsing a request and after structured request strings have been decoded.
function normalizeFunctionRenderingAddress(value) {
  // Callable names can include a nested `<locals>` segment, so stop only at
  // the fixed ` at 0x...>` suffix rather than at the first closing bracket.
  return value.replace(/((?:<|\\u003c)function [^\n]*? at )0x[0-9a-f]+(>|\\u003e)/gi, "$1<asb-address>$2");
}

// Python object reprs have the same process-local address volatility, but are
// a distinct rendering form from callables.  Keep this intentionally narrow:
// only a complete one-line `<qualified.name object at 0x...>` representation
// is normalized; surrounding output and the object's rendered class remain
// significant.
function normalizeObjectRenderingAddress(value) {
  return value.replace(/(<[^<>\n]+ object at )0x[0-9a-f]+>/gi, "$1<asb-address>>");
}

// Hermes creates a one-shot verification script under its private temp root.
// Its random basename is a run-local artifact path, not tool-result content.
// Keep the rule confined to that exact Hermes filename convention; other temp
// paths, filenames, extensions, and all surrounding output remain strict.
function normalizeHermesVerificationScriptPath(value) {
  return value.replace(
    /\/opt\/asb-private\/agent-state\/tmp\/hermes-verify-[A-Za-z0-9]+\.py/g,
    "<asb-hermes-verification-script>",
  );
}

// OpenJiuwen's local vision-capability probe writes its generated image into a
// per-process temporary directory. The probe result forwarded in the next
// provider request includes that generated filename. Its 12-hex suffix is a
// local artifact identity rather than probe or workload content, so preserve
// the complete OpenJiuwen-specific filename convention while canonicalizing
// only that suffix. Other paths, extensions, and all surrounding output stay
// cassette-significant.
function normalizeOpenJiuwenVisionProbeOutputPath(value) {
  return value.replace(
    /\bopenjiuwen(?:_[A-Za-z0-9]+)+\/[A-Za-z0-9]+_[0-9a-f]{12}\.[A-Za-z0-9]+\b/gi,
    (path) => path.replace(/_[0-9a-f]{12}(?=\.[A-Za-z0-9]+$)/i, "_<asb-openjiuwen-probe-output>"),
  );
}

function normalizeCassetteBody(body) {
  if (!body) return body;
  if (typeof body !== "string") body = JSON.stringify(body);
  const workspace = process.env.ASB_TAPE_WORKSPACE;
  const callIdentifiers = new Map();
  const normalized = normalizeFunctionRenderingAddress(normalizePytestElapsed(
    normalizeOpenJiuwenVisionProbeOutputPath(
    normalizeWorkspacePaths(normalizeSseWorkspaceFragments(body, workspace)
      .replaceAll(workspace || "\u0000", "<asb-workspace>"))
    // Preserve distinct call IDs before the generic UUID normalizer below.
    // OpenCode uses `call-<uuid>` while some OpenAI-compatible clients use
    // `call_<uuid>`; both forms are provider-generated rather than prompt
    // semantics, but their relationship within a turn is significant.
    .replace(/"(call[-_][A-Za-z0-9_-]+)"(?=\s*[,}\]])/g, (_, identifier) => {
      if (!callIdentifiers.has(identifier)) {
        callIdentifiers.set(identifier, `<asb-call-${callIdentifiers.size + 1}>`);
      }
      return `"${callIdentifiers.get(identifier)}"`;
    })
    // Agent SDKs generate fresh message/session IDs for every invocation;
    // those identifiers are not provider semantics and must not prevent a
    // cassette recorded in one process from replaying in another.
    .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, "<asb-uuid>")
    .replace(/\b1[0-9]{12,}\b/g, "<asb-timestamp>")
    // Shell directory listings include filesystem mtimes. They are not
    // provider input and naturally differ between record and replay.
    .replace(/\b(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d{1,2} \d{2}:\d{2}\b/g, "<asb-file-mtime>")
    .replace(/\b20\d{2}-\d{2}-\d{2}[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?\b/g, "<asb-file-mtime>")
    // WorkBuddy derives a memory-project slug from the absolute workspace;
    // canonicalize that generated slug when record and replay roots differ.
    .replace(/srv-data-projects-[A-Za-z0-9_.-]+-(?:sweep-(?:record|replay)-[A-Za-z0-9_.-]+|workspaces(?:-replay)?-[A-Za-z0-9_.-]+)/g, "<asb-project-workspace>")
    // Codex includes a diagnostic token count in shell-tool output. It is
    // derived from the rendered transcript and can change during replay; it
    // is not provider request semantics.
    .replace(/Original token count: \d+/g, "Original token count: <asb-token-count>")
    .replace(/original token count: \d+/g, "original token count: <asb-token-count>")
    .replace(/Total output lines: \d+/g, "Total output lines: <asb-line-count>")
    .replace(/codex-arg[A-Za-z0-9_-]+/g, "codex-arg<asb-random>")
    // OpenCode includes its process-local delegated-task session ID in a
    // fixed completed-task envelope returned by its own tool dispatcher. The
    // ID is regenerated on replay, while the task state and result remain
    // provider input. Restrict normalization to that exact envelope.
    .replace(/(<task id=\\?")ses_[A-Za-z0-9_-]+(\\?" state=\\?"completed\\?">)/g, "$1<asb-opencode-task-session>$2")
    // Codex's shell transport prefixes each captured command result with a
    // generated chunk identifier and wall-clock duration.  The duration is a
    // client-side rendering observation, not request semantics: the same
    // recorded command necessarily takes a different amount of time while an
    // offline replay is being driven.  Scope the replacement to this complete
    // fixed Codex envelope so a prompt or arbitrary tool result merely
    // mentioning a wall time remains cassette-significant.
    .replace(/(Chunk ID: [^\n]*?(?:\r?\n|\\n)Wall time: )\d+(?:\.\d+)? seconds(?=(?:\r?\n|\\n)Process exited with code \d+)/g, "$1<asb-wall-time> seconds")
    // Normalize generated call identifiers, but retain the relationship
    // between distinct IDs in one request/response. OpenCode can receive
    // multiple tool calls in a streamed response; collapsing every ID into
    // one marker makes its later tool-result turn ambiguous and diverge from
    // the recording. Never replace the JSON field name `call_id` itself.
    // Restore the malformed key emitted by older recordings before replacing
    // value markers.
    .replace(/"call_<asb-call>":/g, '"call_id":')
    .replace(/\bmsg_resp_[A-Za-z0-9_-]+/g, "msg_resp_<asb-response>")
    .replace(/Chunk ID: [0-9a-f]+/gi, "Chunk ID: <asb-chunk>")
    // OpenCode persists truncated command output beneath a timestamp-derived
    // private filename.  Python tracebacks in that output also contain ASLR
    // addresses and pytest's terminal summary reports elapsed time.  They are
    // observations of the same tool result, not provider input semantics;
    // retain all file contents, failure counts, and commands while replacing
    // only those run-local rendering details before cassette comparison.
    .replace(/\/opt\/asb-private\/agent-state\/data\/opencode\/tool-output\/tool_[A-Za-z0-9_-]+/g, "<asb-opencode-tool-output>")
    // Qwen Code writes an oversized tool result to a generated private file
    // and forwards a fixed persisted-output envelope to the model. The file
    // name changes when llmtape materializes a replay response, while the
    // envelope's digest and every retained preview line stay significant.
    .replace(/(Full output saved to: )\/opt\/asb-private\/agent-state\/home\/\.qwen\/tmp\/[0-9a-f]+\/tool-results\/(?:call_[0-9a-f]+|asb-replay-\d+-<asb-timestamp>-\d+)\.txt/g, "$1<asb-qwen-persisted-output>")
    .replace(/((?:\b\d+ (?:failed|passed|skipped|deselected|errors?),? )+in )\d+(?:\.\d+)?s\b/g, "$1<asb-elapsed>")
    // pip's download progress embeds live transfer throughput and ETA after
    // the already-significant artifact size. Keep the package and byte count
    // strict while removing only transport timing from tool history.
    .replace(/(\b\d+(?:\.\d+)?\/\d+(?:\.\d+)? (?:kB|MB|GB) )\d+(?:\.\d+)? (?:kB|MB|GB)\/s eta \d+:\d+:\d+/g, "$1<asb-transfer-rate> eta <asb-transfer-eta>"))));
  try {
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
      // Claude Code embeds a fresh device identifier in metadata.user_id on
      // every process. It is client telemetry, not provider input, and must
      // not make an otherwise identical recording unreplayable. Keep this
      // exception path-scoped so same-named fields in prompts and tool input
      // remain request-significant.
      if (path.length === 2 && path[0] === "metadata" && path[1] === "user_id" && key === "user_id") {
        return "<asb-user-id>";
      }
      // Codex sends process-local turn/session telemetry in client_metadata.
      // It is neither a prompt nor tool input; normalize only this fixed,
      // provider-defined metadata vocabulary so arbitrary client metadata
      // remains request-significant.
      if (path.length === 2 && path[0] === "client_metadata" && [
        "root_turn_id",
        "session_id",
        "thread_id",
        "turn_id",
        "x-codex-installation-id",
        "x-codex-turn-metadata",
        "x-codex-window-id",
      ].includes(path[1]) && key === path[1]) {
        return "<asb-codex-client-metadata>";
      }
      // Codex returns an opaque encrypted continuation token with each tool
      // turn. The next client request reflects that token, but its encoding
      // (including JSON type) is intentionally process-local. It carries no
      // benchmark instruction or tool result, so normalize only this exact
      // Responses input-item field; every neighboring input field remains
      // cassette-significant.
      if (path.length === 3 && path[0] === "input" && /^\d+$/.test(path[1])
          && path[2] === "encrypted_content" && key === "encrypted_content") {
        return "<asb-codex-encrypted-content>";
      }
      // The same Responses input-item vocabulary carries Codex's generated
      // tool-call identity. It is an opaque continuation reference, not a
      // workload-supplied call ID. Scope this to input items so user JSON and
      // nested tool arguments named call_id remain cassette-significant.
      if (path.length === 3 && path[0] === "input" && /^\d+$/.test(path[1])
          && path[2] === "call_id" && key === "call_id") {
        return "<asb-codex-input-call-id>";
      }
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
      // WorkBuddy carries a process-local conversation request identifier in
      // each OpenAI-compatible history item.  It is not a model-visible
      // instruction or result and naturally changes for every replay.
      if (typeof value === "string"
          && path.length === 3
          && path[0] === "messages"
          && /^\d+$/.test(path[1])
          && path[2] === "conversationRequestId"
          && key === "conversationRequestId") {
        return "<asb-workbuddy-conversation-request>";
      }
      if (typeof value === "string") {
        value = normalizeFunctionRenderingAddress(value);
        value = normalizeObjectRenderingAddress(value);
        value = normalizeHermesVerificationScriptPath(value);
        value = normalizeToolSearchOrdering(value);
        value = normalizeLsDiagnosticOrdering(value);
        const hadParentGuidance = value.includes("../") && value.includes("AGENTS.md");
        const cleaned = value.replace(/\n?\.\.\/.*AGENTS\.md\n?/gm, "");
        const withoutGuidance = hadParentGuidance ? cleaned.replace(/\n+$/g, "") : cleaned;
        if (withoutGuidance.includes("Output:\n")) {
          return withoutGuidance.replace(/(Output:\n)([\s\S]*?)(?=\nfatal:|$)/g, (match, prefix, listing) => {
            const marker = "<asb-workspace>\n";
            const markerIndex = listing.indexOf(marker);
            if (markerIndex >= 0) {
              const header = listing.slice(0, markerIndex);
              const lines = listing.slice(markerIndex + marker.length).split("\n");
              if (lines.every((line) => line === "" || line.startsWith("external-workload/"))) {
                return prefix + header + marker + "<asb-workspace-listing>\n";
              }
            }
            return match;
          });
        }
        return normalizePytestElapsed(withoutGuidance);
      }
      if (Array.isArray(value)) return value.map((item, index) => sort(item, String(index), [...path, String(index)]));
      if (value && typeof value === "object") {
        return Object.fromEntries(Object.keys(value).sort().map((child) => [child, sort(value[child], child, [...path, child])]));
      }
      return value;
    };
    return JSON.stringify(sort(JSON.parse(normalized)))
      .replace(/\.\.\/[^"\\]*AGENTS\.md(?:\\n)?/g, "");
  } catch {
    return normalized;
  }
}

// Tool-call arguments in Anthropic SSE responses are split across many
// `input_json_delta` events. A workspace path can therefore be divided across
// event boundaries and cannot be normalized by a regex over the raw SSE text.
// Reassemble each contiguous content block, normalize it, and emit the result
// in the first fragment while retaining the event boundaries for the client.
function normalizeSseWorkspaceFragments(body, workspace, replacement = "<asb-workspace>") {
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
    const value = normalizeWorkspaceText(group.parts.join(""), workspace, replacement);
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

function normalizeWorkspaceText(value, workspace, replacement = "<asb-workspace>") {
  return (workspace ? value.replaceAll(workspace, replacement) : value)
    .replace(/\/?(?:[A-Za-z0-9_.-]+\/)+workspace-concurrency-\d+(?:\.asb-agent-runtime)?/g, replacement)
    .replace(/\/?(?:[A-Za-z0-9_.-]+\/)+workspace(?=\/|["\\])/g, replacement);
}

function requestKey(request) {
  // Match the normalized full request, not just model and endpoint: a changed
  // prompt or tool state must not silently consume an unrelated response.
  return JSON.stringify([request.method, request.url, normalizeCassetteBody(request.body)]);
}

const interactions = mode === "replay"
  ? readFileSync(cassette, "utf8")
      .split(/\r?\n/)
      .filter(Boolean)
      .map((line) => {
        const interaction = JSON.parse(line);
        if (interaction.request) {
          interaction.request.url = requestPath(interaction.request.url);
          interaction.request.body = normalizeCassetteBody(interaction.request.body);
        }
        if (interaction.response?.body) {
          interaction.response.body = normalizeCassetteBody(interaction.response.body);
        }
        return interaction;
      })
  : [];
let cursor = 0;
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
  if (!request.body) {
    return (request.method === "HEAD" || request.method === "GET")
      && /(?:\/api\/hello|\/health(?:z)?|\/models|\/api\/tags)(?:[/?]|$)/.test(request.url || "");
  }
  try {
    const parsed = JSON.parse(request.body);
    if (request.url?.includes("/embeddings") && parsed.model === "text-embedding-3-small") {
      return true;
    }
  } catch {
    // Keep arbitrary malformed bodies fail-closed below.
  }
  // OpenJiuwen performs this local vision-capability check before its first
  // workload request. It has no benchmark semantics and can be omitted when
  // replaying on a provider/agent path that does not repeat the probe.
  return typeof request.body === "string"
    && request.body.includes("What color is this image? Reply with one word.");
}
writeProgress();

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
  const normalizedBody = normalizeRequestBody(request.url || "/", body.toString("utf8"));
  const recordedRequest = {
    method: request.method || "GET",
    // Some SDKs send an absolute URL even when configured for a proxy. Keep
    // only the path in the cassette and always route the request to the
    // selected upstream; this prevents a client from bypassing the loopback
    // recorder and makes separate-directory replay path-stable.
    url: requestPath(request.url),
    headers: redactHeaders(Object.fromEntries(Object.entries(request.headers))),
    body: body.length ? normalizeCassetteBody(normalizedBody) : null,
  };

  if (mode === "replay") {
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
      writeProgress();
      await replayResponse(response, probe);
      return;
    }
    // A provider bridge may record a health/model probe that the agent omits
    // on replay. Consume only these known, credential-free probes when the
    // next request carries an LLM body; arbitrary mismatches stay fail-closed.
    while (interactions[cursor]?.request
      && body.length > 0
      && isReplayProbe(interactions[cursor].request)
      // If this client did issue the next probe, it must consume its own
      // recorded exchange. Only probes absent from the client request may be
      // skipped.
      && requestKey(recordedRequest) !== requestKey(interactions[cursor].request)) {
      cursor += 1;
      writeProgress();
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
        if (requestKey(recordedRequest) === requestKey(interactions[index].request)) {
          retry = interactions[index];
          break;
        }
      }
      if (retry) {
        await replayResponse(response, retry);
        return;
      }
      const mismatch = requestMismatch(interaction.request, recordedRequest, cursor + 1);
      retainMismatchEvidence(interaction.request, recordedRequest, mismatch);
      reject(mismatch);
      response.writeHead(409, { "content-type": "application/json" });
      response.end(JSON.stringify({ error: "request does not match cassette" }));
      return;
    }
    cursor += 1;
    writeProgress();
    const delayMs = Number(interaction.delay_ms);
    if (respectRecordTimings && Number.isFinite(delayMs) && delayMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, Math.min(delayMs, MAX_DELAY_MS)));
    }
    await replayResponse(response, interaction);
    return;
  }

  // Preserve provider-specific prefixes (e.g. OpenRouter's /api) while
  // keeping the selected upstream host, even for absolute-form requests.
  const target = new URL(`${upstream.replace(/\/+$/, "")}${recordedRequest.url}`);
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
    body: normalizeCassetteBody(responseBody),
  };
  const delayMs = Math.min(Math.max(0, Math.round(performance.now() - startedAt)), MAX_DELAY_MS);
  const interaction = {
    request: recordedRequest,
    response: recordedResponse,
    delay_ms: delayMs,
    timing: {
      request_received_unix_ms: requestReceivedUnixMs,
      response_observed_unix_ms: Math.round(performance.timeOrigin + performance.now()),
    },
  };
  if (capture) capture.append(recording.token, interaction, !isReplayProbe(recordedRequest));
  else {
    if (stopping) return;
    appendFileSync(cassette, `${JSON.stringify(interaction)}\n`);
  }
  response.writeHead(recordedResponse.status, recordedResponse.headers);
  response.end(responseBody);
}

const server = http.createServer((request, response) => {
  const fail = (error) => {
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
  try { token = capture.begin(); }
  catch (error) { fail(error); return; }
  if (!token) {
    response.writeHead(503, { connection: "close" });
    response.end("recording is draining");
    return;
  }
  const recording = { token, request, response, controller: new AbortController() };
  // Subscribe before handle can finish or close a response. Completion requires
  // both full capture and positive delivery, not merely an upstream EOF.
  const delivered = responseDelivery(response);
  activeCaptures.add(recording);
  recording.done = (async () => {
    let success = false;
    try {
      await handle(request, response, recording);
      success = await delivered;
    } catch (error) {
      fail(error);
    } finally {
      capture.end(token, success);
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
