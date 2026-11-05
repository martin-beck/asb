// Claude Code creates a per-process telemetry identifier in this exact
// provider metadata field.  Nothing else in the request is changed.
export function normalizeRequest(request) {
  const body = request?.body;
  if (body?.metadata && typeof body.metadata === "object" && !Array.isArray(body.metadata)
      && typeof body.metadata.user_id === "string") {
    return { ...body, metadata: { ...body.metadata, user_id: "<asb-user-id>" } };
  }
  return body;
}
