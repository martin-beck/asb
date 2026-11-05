// Hermes creates this one-shot local verification path during execution. It
// is not a prompt, argument, or result value and is meaningful only to the
// local client process.
export function normalizeRequest(request) {
  // The legacy recursive rule cannot tell a local path from literal prompt
  // or argument text. Profile-enabled recordings retain it verbatim.
  if (request?.profileActive) return request.body;
  const visit = (value) => {
    if (typeof value === "string") return value.replace(
      /\/opt\/asb-private\/agent-state\/tmp\/hermes-verify-[A-Za-z0-9]+\.py/g,
      "<asb-hermes-verification-script>",
    );
    if (Array.isArray(value)) return value.map(visit);
    if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, visit(item)]));
    return value;
  };
  return visit(request?.body);
}
