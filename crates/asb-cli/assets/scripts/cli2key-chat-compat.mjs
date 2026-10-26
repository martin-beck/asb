// OpenAI Chat Completions <-> Responses compatibility for cli2key.  The
// bridge intentionally implements Responses, while several valid ASB agents
// use the older Chat Completions wire protocol.

function object(value) {
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

function textContent(content, type) {
  if (typeof content === "string") return [{ type, text: content }];
  if (!Array.isArray(content)) return [];
  return content.flatMap((part) => {
    const item = object(part);
    if (item.type === "text" || item.type === "input_text" || item.type === "output_text") {
      return typeof item.text === "string" ? [{ type, text: item.text }] : [];
    }
    if (item.type === "image_url" && item.image_url?.url) return [{ type: "input_image", image_url: item.image_url.url }];
    return [];
  });
}

function instructionText(content) {
  return textContent(content, "input_text")
    .filter((part) => typeof part.text === "string")
    .map((part) => part.text)
    .join("\n");
}

// A continued Responses conversation may contain an opaque reasoning item.
// Codex Bridge accepts that item only when the provider-issued identity and
// encrypted continuation payload travel together.  Some OpenAI-compatible
// clients replay a display-only reasoning shell instead; forwarding that
// shell makes the bridge reject the complete request.  It carries no tool
// call or user-visible content, so drop only the incomplete form while
// preserving a provider-issued opaque continuation verbatim.
function supportedReasoningItem(item) {
  return item?.type !== "reasoning" ||
    (typeof item.id === "string" && typeof item.encrypted_content === "string");
}

// Responses accepts input_text (and input_image) in request input.  Some
// OpenAI-compatible clients use the older generic "text" label when talking
// to /responses.  Normalize it at the provider edge before forwarding to the
// bridge; do not make individual agent adapters carry bridge syntax.
function normalizeInputContent(content, role) {
  if (Array.isArray(content)) return content.map((part) => normalizeInputContent(part, role));
  const part = object(content);
  if (!Object.keys(part).length) return content;
  const normalizedRole = typeof part.role === "string" ? part.role : role;
  const normalized = Object.fromEntries(Object.entries(part).map(([key, value]) => [
    key,
    key === "type" && (value === "text" || value === "input_text" || value === "output_text") &&
      typeof part.text === "string"
      ? (normalizedRole === "assistant" ? "output_text" : "input_text")
      : normalizeInputContent(value, normalizedRole),
  ]));
  return normalized;
}

// Codex Bridge carries system policy through the Responses `instructions`
// field. It deliberately rejects system/developer messages in `input`.
// Normalize that bridge-specific restriction at the cli2key provider edge so
// adapters remain independent of the selected provider.
export function normalizeResponseInstructions(body) {
  const response = object(body);
  if (!Array.isArray(response.input)) return response;
  const instructions = typeof response.instructions === "string" && response.instructions.trim()
    ? [response.instructions]
    : [];
  const input = [];
  for (const item of response.input) {
    const message = object(item);
    if (!supportedReasoningItem(message)) continue;
    if ((message.type === undefined || message.type === "message") &&
        (message.role === "system" || message.role === "developer")) {
      const text = instructionText(message.content);
      if (text) {
        instructions.push(text);
        continue;
      }
    }
    // Some clients use non-message Responses input item wrappers around text
    // content.  Normalize every retained input item, not only messages.
    input.push(normalizeInputContent(message, message.role));
  }
  return {
    ...response,
    input,
    ...(instructions.length ? { instructions: instructions.join("\n\n") } : {}),
  };
}

function inputItems(messages) {
  const input = [];
  for (const raw of Array.isArray(messages) ? messages : []) {
    const message = object(raw);
    const role = message.role;
    if (role === "tool" && typeof message.tool_call_id === "string") {
      input.push({ type: "function_call_output", call_id: message.tool_call_id, output: message.content ?? "" });
      continue;
    }
    if (!["system", "developer", "user", "assistant"].includes(role)) continue;
    const content = textContent(message.content, role === "assistant" ? "output_text" : "input_text");
    if (content.length) input.push({ type: "message", role, content });
    for (const call of Array.isArray(message.tool_calls) ? message.tool_calls : []) {
      const functionCall = object(call).function;
      if (call?.type === "function" && typeof call.id === "string" && functionCall && typeof functionCall.name === "string") {
        input.push({ type: "function_call", call_id: call.id, name: functionCall.name, arguments: typeof functionCall.arguments === "string" ? functionCall.arguments : "{}" });
      }
    }
  }
  return input;
}

export function chatToResponses(body) {
  const chat = object(body);
  const result = {
    model: chat.model,
    input: inputItems(chat.messages),
    stream: Boolean(chat.stream),
    store: false,
  };
  if (!result.input.length) result.input = [{ type: "message", role: "user", content: [{ type: "input_text", text: "" }] }];
  if (Array.isArray(chat.tools)) {
    result.tools = chat.tools.flatMap((tool) => {
      const fn = object(tool).function;
      return tool?.type === "function" && typeof fn.name === "string"
        ? [{ type: "function", name: fn.name, description: typeof fn.description === "string" ? fn.description : "", parameters: object(fn.parameters) }]
        : [];
    });
  }
  if (typeof chat.max_tokens === "number") result.max_output_tokens = chat.max_tokens;
  if (typeof chat.temperature === "number") result.temperature = chat.temperature;
  if (typeof chat.reasoning_effort === "string") result.reasoning = { effort: chat.reasoning_effort };
  return normalizeResponseInstructions(result);
}

function usage(response) {
  const source = object(response.usage);
  return {
    prompt_tokens: source.input_tokens ?? 0,
    completion_tokens: source.output_tokens ?? 0,
    total_tokens: source.total_tokens ?? ((source.input_tokens ?? 0) + (source.output_tokens ?? 0)),
  };
}

function message(response) {
  const output = Array.isArray(response.output) ? response.output : [];
  const content = output.flatMap((item) => Array.isArray(item?.content) ? item.content : [])
    .filter((part) => part?.type === "output_text")
    .map((part) => part.text ?? "")
    .join("");
  const tool_calls = output.filter((item) => item?.type === "function_call")
    .map((item, index) => ({ id: item.call_id ?? item.id ?? `call_${index}`, type: "function", function: { name: item.name, arguments: item.arguments ?? "{}" } }));
  return { role: "assistant", content: content || null, ...(tool_calls.length ? { tool_calls } : {}) };
}

export function responseToChatCompletion(response) {
  const value = object(response);
  const choiceMessage = message(value);
  return {
    id: value.id ?? "chatcmpl-cli2key",
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model: value.model ?? "cli2key",
    choices: [{ index: 0, message: choiceMessage, finish_reason: choiceMessage.tool_calls ? "tool_calls" : "stop" }],
    usage: usage(value),
  };
}

export function responseEventToChatChunk(event, model) {
  const value = object(event);
  const response = object(value.response);
  const base = { id: response.id ?? value.response_id ?? "chatcmpl-cli2key", object: "chat.completion.chunk", created: Math.floor(Date.now() / 1000), model: response.model ?? model ?? "cli2key" };
  if (value.type === "response.output_text.delta") return { ...base, choices: [{ index: value.output_index ?? 0, delta: { content: value.delta ?? "" }, finish_reason: null }] };
  if (value.type === "response.output_item.done" && value.item?.type === "function_call") {
    const item = value.item;
    return { ...base, choices: [{ index: value.output_index ?? 0, delta: { tool_calls: [{ index: 0, id: item.call_id ?? item.id, type: "function", function: { name: item.name, arguments: item.arguments ?? "{}" } }] }, finish_reason: null }] };
  }
  if (value.type === "response.completed") {
    const completed = responseToChatCompletion(response);
    return { ...base, choices: [{ index: 0, delta: {}, finish_reason: completed.choices[0].finish_reason }], usage: completed.usage };
  }
  return null;
}
