// Extract only protocol-defined tool-result text from a comparison copy.
// Identifiers are associated before any cassette identifier canonicalization;
// duplicated IDs are deliberately ambiguous rather than guessed.
function association(calls, id) {
  if (typeof id !== "string") return null;
  const call = calls.get(id);
  if (!call || typeof call.name !== "string") return null;
  let args = call.arguments;
  if (typeof args === "string") {
    try { args = JSON.parse(args); } catch { args = null; }
  }
  if (!args || typeof args !== "object" || Array.isArray(args)) args = null;
  return { name: call.name, arguments: args };
}

function rememberCall(calls, call) {
  if (typeof call.id !== "string") return;
  calls.set(call.id, calls.has(call.id) ? null : call);
}

function textBlocks(content, apply, tool, location) {
  if (typeof content === "string") return apply(content, tool, location);
  if (!Array.isArray(content)) return content;
  return content.map((block, index) => {
    if (!block || typeof block !== "object" || Array.isArray(block)) return block;
    if ((block.type === "text" || block.type === "input_text" || block.type === "output_text") && typeof block.text === "string") {
      return { ...block, text: apply(block.text, tool, `${location}/${index}/text`) };
    }
    return block;
  });
}

/** Return a comparison copy; callback is invoked only for recognized results. */
export function normalizeToolResults(body, transform) {
  if (!body || typeof body !== "object" || Array.isArray(body)) return body;
  const result = structuredClone(body);
  const apply = (text, tool, location) => {
    const value = transform({ text, tool, location });
    if (typeof value !== "string") throw new TypeError("tool-result normalizer must return a string");
    return value;
  };
  if (Array.isArray(result.input)) {
    const calls = new Map();
    for (const [index, item] of result.input.entries()) {
      if (item?.type === "function_call") rememberCall(calls, { id: item.call_id, name: item.name, arguments: item.arguments });
      if (item?.type === "function_call_output" && Object.hasOwn(item, "output")) item.output = textBlocks(item.output, apply, association(calls, item.call_id), `input/${index}/output`);
    }
  }
  if (Array.isArray(result.messages)) {
    const calls = new Map();
    for (const [index, message] of result.messages.entries()) {
      if (!message || typeof message !== "object" || Array.isArray(message)) continue;
      if (message.role === "assistant" && Array.isArray(message.tool_calls)) {
        for (const call of message.tool_calls) rememberCall(calls, { id: call?.id, name: call?.function?.name, arguments: call?.function?.arguments });
      }
      if (message.role === "tool" && Object.hasOwn(message, "content")) message.content = textBlocks(message.content, apply, association(calls, message.tool_call_id), `messages/${index}/content`);
      if (Array.isArray(message.content)) {
        message.content = message.content.map((block, blockIndex) => {
          if (message.role === "assistant" && block?.type === "tool_use") rememberCall(calls, { id: block.id, name: block.name, arguments: block.input });
          return message.role === "user" && block?.type === "tool_result" && Object.hasOwn(block, "content")
            ? { ...block, content: textBlocks(block.content, apply, association(calls, block.tool_use_id), `messages/${index}/content/${blockIndex}/content`) }
            : block;
        });
      }
    }
  }
  return result;
}

/** Apply a supervised asynchronous normalizer in deterministic block order. */
export async function normalizeToolResultsAsync(body, transform, { maxTotalMs = 5000, maxResults = 4096, maxOutputBytes = 16 * 1024 * 1024, associationBody = body } = {}) {
  const started = performance.now();
  const contexts = [];
  normalizeToolResults(body, (context) => {
    if (contexts.length >= maxResults) throw new Error("normalizer request has too many tool-result blocks");
    contexts.push(context);
    return context.text;
  });
  // Capture associations before agent envelope normalization can replace or
  // collapse protocol IDs. It may change result text but must preserve the
  // locations of recognized result blocks for this association to be
  // meaningful; never guess if the envelope no longer corresponds.
  const associations = [];
  normalizeToolResults(associationBody, (context) => {
    if (associations.length >= maxResults) throw new Error("normalizer request has too many tool-result blocks");
    associations.push({ tool: context.tool, location: context.location });
    return context.text;
  });
  if (associations.length !== contexts.length || contexts.some((context, index) => context.location !== associations[index].location)) {
    throw new Error("agent envelope changed tool-result boundaries before normalization");
  }
  const outputs = [];
  for (const [index, context] of contexts.entries()) {
    if (performance.now() - started >= maxTotalMs) throw new Error("normalizer request exceeded its time limit");
    const value = await transform({ text: context.text, tool: associations[index].tool });
    if (performance.now() - started >= maxTotalMs) throw new Error("normalizer request exceeded its time limit");
    if (typeof value !== "string") throw new TypeError("tool-result normalizer must return a string");
    outputs.push(value);
  }
  let index = 0;
  const result = normalizeToolResults(body, () => outputs[index++]);
  if (Buffer.byteLength(JSON.stringify(result)) > maxOutputBytes) throw new Error("normalized request exceeds the output limit");
  return result;
}
