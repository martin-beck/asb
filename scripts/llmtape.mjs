#!/usr/bin/env node

// Small project-local HTTP recorder/replayer embedded in ASB. It supplies the
// bounded loopback protocol that ASB's tape module invokes.

import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import http from "node:http";
import { performance } from "node:perf_hooks";

const MAX_BODY = 16 * 1024 * 1024;
const MAX_DELAY_MS = 60 * 60 * 1000;

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
if (!Number.isInteger(port) || port < 1 || port > 65535) usage("--port must be a valid TCP port");
const cassette = mode === "record" ? option(args, "--out") : option(args, "--cassette");
const upstream = mode === "record" ? option(args, "--upstream") : undefined;
const respectRecordTimings = args.includes("--respect-record-timings");
if (respectRecordTimings && mode !== "replay") {
  usage("--respect-record-timings is only valid with replay");
}

function boundedBody(chunks) {
  const body = Buffer.concat(chunks);
  if (body.length > MAX_BODY) throw new Error("request body exceeds 16 MiB");
  return body;
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
    if (!/^(connection|content-length|transfer-encoding)$/i.test(name)) result[name] = value;
  }
  return result;
}

// Preserve server-sent event boundaries during replay.  Several agent CLIs
// dispatch tool calls as soon as the terminating SSE event arrives; sending
// the complete cassette body as one write can make those clients finish the
// turn before their tool dispatcher observes the event sequence.
async function replayResponse(response, interaction) {
  const body = interaction.response.body || "";
  const contentType = Object.entries(interaction.response.headers || {})
    .find(([name]) => name.toLowerCase() === "content-type")?.[1] || "";
  response.writeHead(interaction.response.status, interaction.response.headers || {});
  if (!/text\/event-stream/i.test(contentType) || !body.includes("\n\n")) {
    response.end(body);
    return;
  }
  const events = body.match(/[\s\S]*?(?:\r?\n\r?\n|$)/g)?.filter(Boolean) || [body];
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
  if (!body || (!url.includes("/messages") && !url.includes("/responses"))) return body;
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
function normalizeCassetteBody(body) {
  if (!body) return body;
  const normalized = body
    .replace(
    /\/(?:srv\/data\/projects|tmp)\/[A-Za-z0-9._~+%-]+(?:\/[A-Za-z0-9._~+%-]+)*/g,
    "<asb-path>",
    )
    // Agent SDKs generate fresh message/session IDs for every invocation;
    // those identifiers are not provider semantics and must not prevent a
    // cassette recorded in one process from replaying in another.
    .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, "<asb-uuid>")
    .replace(/\b1[0-9]{12,}\b/g, "<asb-timestamp>");
  try {
    const sort = (value) => {
      if (Array.isArray(value)) return value.map(sort);
      if (value && typeof value === "object") {
        return Object.fromEntries(Object.keys(value).sort().map((key) => [key, sort(value[key])]));
      }
      return value;
    };
    return JSON.stringify(sort(JSON.parse(normalized)));
  } catch {
    return normalized;
  }
}

function requestKey(request) {
  let model = null;
  try {
    model = request.body ? JSON.parse(request.body).model || null : null;
  } catch {
    // Non-JSON provider requests still match on method and endpoint below.
  }
  // The ordered cassette already scopes one provider/model run.  SDK request
  // bodies contain ephemeral session, tool-state, and prompt metadata that
  // legitimately changes between record and replay.  Require the stable wire
  // contract (method, endpoint path, and model), then consume interactions in
  // order so a request to the wrong provider route remains fail-closed.
  return JSON.stringify([request.method, request.url, model]);
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
        return interaction;
      })
  : [];
let cursor = 0;
const progressPath = `${cassette}.progress.json`;
function writeProgress() {
  if (mode === "replay") {
    writeFileSync(progressPath, JSON.stringify({ schema_version: 1, consumed: cursor, total: interactions.length }));
  }
}
writeProgress();

async function handle(request, response) {
  const startedAt = performance.now();
  const chunks = [];
  for await (const chunk of request) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  const body = boundedBody(chunks);
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
    const interaction = interactions[cursor];
    if (!interaction) {
      response.writeHead(409, { "content-type": "application/json" });
      response.end(JSON.stringify({ error: "cassette has no more recorded requests" }));
      return;
    }
    if (requestKey(recordedRequest) !== requestKey(interaction.request)) {
      if (process.env.ASB_TAPE_DEBUG === "1") {
        writeFileSync("/tmp/asb-tape-expected.json", interaction.request.body || "");
        writeFileSync("/tmp/asb-tape-actual.json", recordedRequest.body || "");
      }
      response.writeHead(409, { "content-type": "application/json" });
      response.end(JSON.stringify({
        error: "request does not match cassette",
        ...(process.env.ASB_TAPE_DEBUG === "1"
          ? {
              expected: {
                method: interaction.request.method,
                url: interaction.request.url,
                body_length: interaction.request.body?.length ?? 0,
                body_prefix: interaction.request.body?.slice(0, 500) ?? null,
              },
              actual: {
                method: recordedRequest.method,
                url: recordedRequest.url,
                body_length: recordedRequest.body?.length ?? 0,
                body_prefix: recordedRequest.body?.slice(0, 500) ?? null,
              },
            }
          : {}),
      }));
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

  const target = new URL(recordedRequest.url, upstream);
  const forwardedHeaders = { ...request.headers };
  delete forwardedHeaders.host;
  delete forwardedHeaders.connection;
  delete forwardedHeaders["content-length"];
  const upstreamResponse = await fetch(target, {
    method: recordedRequest.method,
    headers: forwardedHeaders,
    body: body.length ? Buffer.from(normalizedBody) : undefined,
  });
  const responseBody = await upstreamResponse.text();
  const recordedResponse = {
    status: upstreamResponse.status,
    headers: responseHeaders(upstreamResponse.headers),
    body: responseBody,
  };
  const delayMs = Math.min(Math.max(0, Math.round(performance.now() - startedAt)), MAX_DELAY_MS);
  appendFileSync(cassette, `${JSON.stringify({ request: recordedRequest, response: recordedResponse, delay_ms: delayMs })}\n`);
  response.writeHead(recordedResponse.status, recordedResponse.headers);
  response.end(responseBody);
}

const server = http.createServer((request, response) => {
  handle(request, response).catch((error) => {
    response.writeHead(502, { "content-type": "application/json" });
    response.end(JSON.stringify({ error: String(error.message || error) }));
  });
});

if (mode === "record") {
  if (existsSync(cassette)) writeFileSync(cassette, "");
}
server.listen(port, "127.0.0.1");
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
