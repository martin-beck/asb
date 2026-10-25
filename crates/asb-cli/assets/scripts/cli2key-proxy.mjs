#!/usr/bin/env node

// Provider-neutral compatibility hop for the project-local cli2key bridge.
// Codex can include host-only `additional_tools` input items which the bridge
// intentionally does not expose.  Strip only that metadata before forwarding;
// all actual prompts and function tools remain unchanged.

import http from "node:http";
import { Readable } from "node:stream";
import { fstatSync } from "node:fs";
import { cancelNetworkRequests, networkFetch, quiesceNetworkRequests } from "./network-fetch.mjs";
import { chatToResponses, normalizeResponseInstructions, responseEventToChatChunk, responseToChatCompletion } from "./cli2key-chat-compat.mjs";

function usage(message) {
  if (message) console.error(`asb cli2key proxy: ${message}`);
  console.error("usage: cli2key-proxy --port PORT --upstream URL");
  process.exit(2);
}

const args = process.argv.slice(2);
const value = (name) => {
  const index = args.indexOf(name);
  if (index < 0 || !args[index + 1] || args[index + 1].startsWith("--")) {
    usage(`${name} requires a value`);
  }
  return args[index + 1];
};
const listenerFd = Number(value("--listen-fd"));
const execHelper = value("--exec-helper");
const upstream = value("--upstream").replace(/\/$/, "");
if (!Number.isInteger(listenerFd) || listenerFd < 3) usage("--listen-fd must be an inherited socket descriptor");
function ownedIdentity(fd, kind) {
  const stat = fstatSync(fd, { bigint: true });
  if ((kind === "socket" && !stat.isSocket()) || (kind === "file" && !stat.isFile())) {
    throw new Error("invalid owned descriptor kind");
  }
  return { fd, device: stat.dev.toString(), inode: stat.ino.toString(), kind };
}
let outboundLaunch;
try {
  const owned = [ownedIdentity(listenerFd, "socket")];
  if (args.includes("--owned-image-fd")) {
    const fd = Number(value("--owned-image-fd"));
    if (!Number.isInteger(fd) || fd < 3 || fd === listenerFd) throw new Error("invalid image descriptor");
    owned.push(ownedIdentity(fd, "file"));
  }
  outboundLaunch = Object.freeze({ program: execHelper,
    args: Object.freeze(["__asb-owned-fd-exec", JSON.stringify(owned), "--"]) });
} catch {
  usage("invalid owned descriptor handoff");
}

function normalizeBody(path, body) {
  if (!body || !path.includes("/responses")) return body;
  try {
    const parsed = JSON.parse(body);
    let changed = false;
    if (Array.isArray(parsed.input)) {
      const filtered = parsed.input.filter((item) => item?.type !== "additional_tools");
      changed = filtered.length !== parsed.input.length;
      parsed.input = filtered;
    }
    if (parsed.reasoning && typeof parsed.reasoning === "object" &&
        Object.hasOwn(parsed.reasoning, "context")) {
      delete parsed.reasoning.context;
      changed = true;
    }
    return changed ? JSON.stringify(parsed) : body;
  } catch {
    return body;
  }
}

let closing = false;
const server = http.createServer(async (request, response) => {
  try {
    if (closing) { response.destroy(); return; }
    const chunks = [];
    for await (const chunk of request) chunks.push(Buffer.from(chunk));
    if (closing || request.aborted || response.destroyed) return;
    const raw = Buffer.concat(chunks);
    const path = request.url || "/";
    const chat = path === "/v1/chat/completions";
    const rawBody = raw.length ? raw.toString("utf8") : "";
    const normalized = normalizeBody(path, rawBody);
    let translated = null;
    let body = normalized;
    if (chat) {
      translated = chatToResponses(JSON.parse(rawBody));
      body = JSON.stringify(translated);
    } else if (path.includes("/responses") && normalized) {
      try {
        body = JSON.stringify(normalizeResponseInstructions(JSON.parse(normalized)));
      } catch {
        // Preserve malformed input for the upstream bridge to classify as a
        // client request error instead of converting it into a proxy failure.
      }
    }
    const target = new URL(chat ? "/v1/responses" : path, `${upstream}/`);
    const headers = { ...request.headers };
    delete headers.host;
    delete headers.connection;
    delete headers["content-length"];
    const result = await networkFetch(target, {
      method: request.method,
      headers,
      body: body || undefined,
    }, 0, outboundLaunch);
    if (chat && result.status >= 200 && result.status < 300) {
      if (translated.stream) {
        response.writeHead(result.status, { "content-type": "text/event-stream; charset=utf-8", "cache-control": "no-cache" });
        let pending = "";
        for await (const chunk of Readable.fromWeb(result.body)) {
          pending += Buffer.from(chunk).toString("utf8");
          let boundary;
          while ((boundary = pending.match(/\r?\n\r?\n/))) {
            const separator = boundary.index;
            const frame = pending.slice(0, separator);
            pending = pending.slice(separator + boundary[0].length);
            const data = frame.split(/\r?\n/).filter((line) => line.startsWith("data:")).map((line) => line.slice(5).trimStart()).join("\n");
            if (!data || data === "[DONE]") continue;
            const chunkValue = responseEventToChatChunk(JSON.parse(data), translated.model);
            if (chunkValue) response.write(`data: ${JSON.stringify(chunkValue)}\n\n`);
          }
        }
        response.end("data: [DONE]\n\n");
        return;
      }
      const responseBody = await new Response(result.body).json();
      response.writeHead(result.status, { "content-type": "application/json" });
      response.end(JSON.stringify(responseToChatCompletion(responseBody)));
      return;
    }
    response.writeHead(result.status, Object.fromEntries(result.headers));
    if (result.body) {
      const bodyStream = Readable.fromWeb(result.body);
      bodyStream.on("error", () => response.destroy());
      response.on("close", () => bodyStream.destroy());
      bodyStream.pipe(response);
    }
    else response.end();
  } catch (error) {
    if (closing || request.aborted || response.destroyed || response.writableEnded) return;
    response.writeHead(502, { "content-type": "application/json" });
    response.end(JSON.stringify({ error: { message: `cli2key upstream unavailable: ${error.message}` } }));
  }
});

server.on("error", (error) => {
  console.error(`asb cli2key proxy: ${error.message}`);
  process.exit(1);
});
server.listen({ fd: listenerFd }, () => {
  const address = server.address();
  if (!address || typeof address === "string" || address.address !== "127.0.0.1" || address.port < 1) {
    console.error("asb cli2key proxy: invalid inherited listener address");
    process.exit(1);
  }
  console.log(`ASB cli2key proxy ready ${JSON.stringify({schema_version:1,host:address.address,port:address.port})}`);
});
const shutdown = async () => {
  if (closing) return;
  closing = true;
  const closed = new Promise((resolve) => server.close(resolve));
  cancelNetworkRequests();
  try {
    // Observe owned subprocess close before exiting; a resistant curl receives
    // the existing bounded TERM/KILL escalation in the network client.
    await quiesceNetworkRequests();
    server.closeAllConnections();
    await closed;
    process.exit(0);
  } catch {
    server.closeAllConnections();
    process.exit(1);
  }
};
process.once("SIGINT", shutdown);
process.once("SIGTERM", shutdown);
process.stdin.once("end", shutdown);
process.stdin.once("error", shutdown);
process.stdin.resume();
