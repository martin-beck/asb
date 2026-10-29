// CodeBuddy runs in a SWE-bench guest where the benchmark workspace is mounted
// at /testbed. ASB's sealed local-tool service accepts workspace-relative
// paths, so this adapter-owned stdio bridge translates only that guest view.
import { spawn } from "node:child_process";
import readline from "node:readline";

let child;
try {
  child = JSON.parse(process.argv[2] || "");
} catch {
  child = null;
}
if (!Array.isArray(child) || !child.length || !child.every((value) => typeof value === "string" && value)) {
  process.stderr.write("ASB WorkBuddy MCP bridge has no child command\n");
  process.exit(64);
}

function relativeTestbed(value) {
  if (typeof value !== "string") return value;
  if (value === "/testbed" || value === "/testbed/") return ".";
  if (!value.startsWith("/testbed/")) return value;
  const relative = value.slice("/testbed/".length);
  if (!relative || relative.split("/").some((part) => !part || part === "." || part === "..")) {
    return null;
  }
  return relative;
}

function rewriteCall(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)
      || value.method !== "tools/call" || !value.params
      || typeof value.params !== "object" || Array.isArray(value.params)
      || typeof value.params.name !== "string" || !value.params.arguments
      || typeof value.params.arguments !== "object" || Array.isArray(value.params.arguments)) {
    return value;
  }
  const arguments_ = { ...value.params.arguments };
  switch (value.params.name) {
    case "list_directory":
    case "read_file":
    case "write_file":
    case "create_directory":
    case "remove_file":
    case "search_text":
      arguments_.path = relativeTestbed(arguments_.path);
      if (arguments_.path === null) {
        return { ...value, __asbWorkBuddyRejectedPath: true };
      }
      break;
    default:
      return value;
  }
  return { ...value, params: { ...value.params, arguments: arguments_ } };
}

const service = spawn(child[0], child.slice(1), { stdio: ["pipe", "pipe", "inherit"] });
service.on("error", (error) => {
  process.stderr.write(`ASB WorkBuddy MCP bridge failed: ${error.message}\n`);
  process.exitCode = 1;
});
service.on("close", (code, signal) => {
  process.exitCode = code === 0 ? 0 : 1;
  if (signal) process.stderr.write("ASB WorkBuddy MCP bridge child stopped\n");
});
service.stdout.pipe(process.stdout);

const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", (line) => {
  try {
    const request = rewriteCall(JSON.parse(line));
    if (request.__asbWorkBuddyRejectedPath) {
      const { __asbWorkBuddyRejectedPath, ...safe } = request;
      process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: safe.id ?? null, error: {
        code: -32602, message: "ASB WorkBuddy path escapes /testbed",
      } })}\n`);
      return;
    }
    service.stdin.write(`${JSON.stringify(request)}\n`);
  } catch {
    // Preserve malformed protocol frames byte-for-byte so the owned service
    // remains responsible for its normal protocol rejection.
    service.stdin.write(`${line}\n`);
  }
});
input.on("close", () => service.stdin.end());
