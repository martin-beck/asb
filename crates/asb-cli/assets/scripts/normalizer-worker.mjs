// Owned per-session worker. A vm context limits accidental access to ambient
// state; it is not a security boundary for untrusted modules.
import { parentPort, workerData } from "node:worker_threads";
import vm from "node:vm";

const modules = [];
let loadingProfile = "unknown";
try {
  for (const profile of workerData.profiles) {
    loadingProfile = profile.id;
    // The initial ABI is intentionally a single-file synchronous function.
    // Imports and extra exports have no module loader in this execution mode.
    const source = profile.source;
    if (typeof source !== "string" || /\bimport\s*(?:[({*]|[\w$])/.test(source)
        || !/\bexport\s+function\s+normalizeToolResult\s*\(/.test(source)) {
      throw new Error(`normalizer ${profile.id}: expected a single exported normalizeToolResult function without imports`);
    }
    const script = source.replace(/\bexport\s+function\s+normalizeToolResult\s*\(/, "function normalizeToolResult(");
    const context = vm.createContext(Object.create(null), { codeGeneration: { strings: false, wasm: false } });
    new vm.Script("globalThis.Date = undefined; Math.random = undefined;").runInContext(context, { timeout: workerData.callTimeoutMs });
    new vm.Script(`${script}\n;globalThis.__asbNormalize = normalizeToolResult;`, { filename: profile.id }).runInContext(context, { timeout: workerData.callTimeoutMs });
    modules.push({ id: profile.id, context });
  }
  parentPort.postMessage({ type: "ready" });
} catch (error) {
  // Source evaluation may throw arbitrary text, including credentials or
  // tool output. The user needs the profile identity and failure category,
  // not the exception contents.
  const message = String(error?.message || "");
  const kind = message.includes("Script execution timed out") ? "timed out during initialization"
    : error instanceof SyntaxError ? "has invalid syntax"
    : "could not be initialized";
  parentPort.postMessage({ type: "failure", message: `normalizer ${loadingProfile}: ${kind}` });
}

parentPort.on("message", ({ id, context }) => {
  let activeProfile = "unknown";
  try {
    let text = context.text;
    const changed = [];
    for (const profile of modules) {
      activeProfile = profile.id;
      // JSON is passed into the context as data, avoiding host objects with
      // attached methods. Each profile sees the same metadata and previous
      // profile's text, never the surrounding request object.
      profile.context.__asbInput = JSON.stringify({ ...context, text });
      const value = new vm.Script("__asbNormalize(JSON.parse(__asbInput))").runInContext(profile.context, { timeout: workerData.callTimeoutMs });
      delete profile.context.__asbInput;
      if (typeof value !== "string") throw new Error(`normalizer ${profile.id}: expected a synchronous string result`);
      if (Buffer.byteLength(value) > workerData.maxOutputBytes) throw new Error(`normalizer ${profile.id}: output exceeds limit`);
      if (value !== text) changed.push(profile.id);
      text = value;
    }
    parentPort.postMessage({ type: "result", id, text, changed });
  } catch (error) {
    // A profile can throw tool text or credentials. Keep diagnostics bounded
    // and identify only the profile and failure class, never exception data.
    const message = String(error?.message || "");
    const kind = message.includes("Script execution timed out") ? "timed out"
      : message.includes("expected a synchronous string result") ? "returned a non-string result"
      : message.includes("output exceeds limit") ? "exceeded the output limit"
      : "raised an exception";
    parentPort.postMessage({ type: "failure", id, message: `normalizer ${activeProfile}: ${kind}` });
  }
});
