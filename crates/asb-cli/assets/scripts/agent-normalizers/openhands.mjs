// OpenHands inserts fresh private observation paths into later history.  Only
// that documented runtime hierarchy is canonicalized; other strings stay
// unchanged.
export function normalizeRequest(request) {
  const visit = (value) => {
    if (typeof value === "string") return value
      .replace(/(\/openhands\/conversations\/)[0-9a-f]{8,}(?:\/subagents\/[0-9a-f]{8,})?/gi, "$1<asb-openhands-conversation>")
      .replace(/(\/openhands\/conversations\/<asb-openhands-conversation>\/observations\/terminal_output_)[0-9a-f]{8,}(\.txt\b)/gi, "$1<asb-openhands-terminal>$2");
    if (Array.isArray(value)) return value.map(visit);
    if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, visit(item)]));
    return value;
  };
  return visit(request?.body);
}
