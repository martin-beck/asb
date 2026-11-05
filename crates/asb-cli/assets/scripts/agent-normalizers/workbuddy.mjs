// CodeBuddy derives these values from its local process and workspace.  The
// script is selected only for the WorkBuddy adapter; model-visible content is
// preserved apart from its generated local workspace slug.
export function normalizeRequest(request) {
  const body = request?.body;
  if (!body || typeof body !== "object" || Array.isArray(body) || !Array.isArray(body.messages)) return body;
  const normalizeString = (value) => value
    .replace(/srv-data-projects-[A-Za-z0-9_.-]+-(?:sweep-(?:record|replay)-[A-Za-z0-9_.-]+|workspaces(?:-replay)?-[A-Za-z0-9_.-]+)/g, "<asb-project-workspace>")
    .replace(/srv-data-projects-[A-Za-z0-9_.-]+-results-asb-executions-[A-Za-z0-9-]+-batch-\d+-workspace/g, "<asb-project-workspace>")
    .replace(/(<codebuddy_background_info>[\s\S]*?model named ).*?(\. The exact model ID is ).*?(?=\.(?:\n|$))/i, "$1<asb-workbuddy-model>$2<asb-workbuddy-model>");
  const visit = (value) => {
    if (typeof value === "string") return normalizeString(value);
    if (Array.isArray(value)) return value.map(visit);
    if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, visit(item)]));
    return value;
  };
  const messages = body.messages.map((message) => {
    if (!message || typeof message !== "object" || Array.isArray(message)) return message;
    const next = { ...message };
    for (const key of ["conversationRequestId", "messageId", "requestModelId", "requestModelName"]) {
      if (typeof next[key] === "string") next[key] = `<asb-workbuddy-${key}>`;
    }
    next.content = visit(next.content);
    return next;
  });
  const normalized = { ...body, messages };
  // CodeBuddy may add this complete local transport-default envelope after a
  // resumed conversation. It is selected only after the adapter has already
  // identified a CodeBuddy request above, never by a shared tape heuristic.
  if (messages.some((message) => JSON.stringify(message?.content || "").includes("You are CodeBuddy Code."))) {
    for (const field of ["cache_retention", "extra_vars", "max_tokens", "reasoning_effort", "reasoning_summary", "temperature", "verbosity"]) {
      delete normalized[field];
    }
  }
  return normalized;
}
