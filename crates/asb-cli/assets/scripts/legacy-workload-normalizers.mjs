import { normalizeOpenAiSseWorkspacePaths, normalizeWorkspacePaths } from "./tape-normalization.mjs";

export function normalizeLegacyStable(value) {
  return value.replace(/\.\.\/[^"\\]*AGENTS\.md(?:\\n)?/g, "");
}

// Compatibility implementation for historical cassette request keys. New
// profile-enabled recordings use explicitly selected tool-result scripts.
// Result workspaces are intentionally different for record and replay.  Keep
// absolute project paths out of cassette keys while forwarding the normalized
// request body unchanged to the live provider during recording.
export function normalizeToolSearchOrdering(value) {
  // OpenCode's built-in repository search can return identical match blocks in
  // filesystem enumeration order. Fresh workload materialization is allowed
  // to change that order, but not the observed paths or lines. Restrict the
  // canonicalization to its fully formed /testbed match report so prompts,
  // commands, and arbitrary tool output remain cassette-significant.
  const header = value.match(/^(Found \d+ matches\n)/);
  if (!header) return value;
  const body = value.slice(header[0].length);
  const trailingNewline = body.endsWith("\n");
  const blocks = body.split(/\n{2,}(?=\/testbed\/)/).map((block) => block.replace(/\n+$/, ""));
  if (blocks.length < 2 || blocks.some((block) => !block.startsWith("/testbed/"))) {
    return value;
  }
  return header[0] + blocks.sort((left, right) => left.localeCompare(right)).join("\n\n")
    + (trailingNewline ? "\n" : "");
}

export function normalizeLegacyRaw(body, workspace, normalizeSseWorkspaceFragments) {
  const callIdentifiers = new Map();
  return normalizeFunctionRenderingAddress(normalizePytestElapsed(
    normalizeWorkspacePaths(normalizeOpenAiSseWorkspacePaths(normalizeSseWorkspaceFragments(body, workspace), workspace)
      .replaceAll(workspace || "\u0000", "<asb-workspace>"))
    // Preserve distinct call IDs before the generic UUID normalizer below.
    // OpenCode uses `call-<uuid>` while some OpenAI-compatible clients use
    // `call_<uuid>`; both forms are provider-generated rather than prompt
    // semantics, but their relationship within a turn is significant.
    .replace(/"(call[-_][A-Za-z0-9_-]+)"(?=\s*[,}\]])/g, (_, identifier) => {
      if (!callIdentifiers.has(identifier)) {
        callIdentifiers.set(identifier, `<asb-call-${callIdentifiers.size + 1}>`);
      }
      return `"${callIdentifiers.get(identifier)}"`;
    })
    // Generated replay protocol IDs contain a local clock; literal timestamps
    // in prompts, tool arguments and tool results remain replay-significant.
    .replace(/(asb-replay-\d+-)1[0-9]{12,}(?=-\d+)/g, "$1<asb-timestamp>")
    // Pytest decorates its terminal summary with ANSI color resets. Preserve
    // every status/count and trailing content, but replace only the elapsed
    // renderer in a complete colored pytest summary.
    .replace(/((?:\d+ (?:passed|failed|skipped|deselected|errors?)(?:, )?)+(?:\\u001b\[(?:0|3[12])m)* in )\d+(?:\.\d+)?s(?=\\u001b\[0m)/g, "$1<asb-pytest-elapsed>")
    // Normalize generated call identifiers, but retain the relationship
    // between distinct IDs in one request/response. OpenCode can receive
    // multiple tool calls in a streamed response; collapsing every ID into
    // one marker makes its later tool-result turn ambiguous and diverge from
    // the recording. Never replace the JSON field name `call_id` itself.
    // Restore the malformed key emitted by older recordings before replacing
    // value markers.
    .replace(/"call_<asb-call>":/g, '"call_id":')
    .replace(/\bmsg_resp_[A-Za-z0-9_-]+/g, "msg_resp_<asb-response>")
    .replace(/Chunk ID: [0-9a-f]+/gi, "Chunk ID: <asb-chunk>")
    .replace(/((?:\b\d+ (?:failed|passed|skipped|deselected|errors?),? )+in )\d+(?:\.\d+)?s\b/g, "$1<asb-elapsed>")
    // pip's download progress embeds live transfer throughput and ETA after
    // the already-significant artifact size. Keep the package and byte count
    // strict while removing only transport timing from tool history.
    .replace(/(\b\d+(?:\.\d+)?\/\d+(?:\.\d+)? (?:kB|MB|GB) )\d+(?:\.\d+)? (?:kB|MB|GB)\/s eta \d+:\d+:\d+/g, "$1<asb-transfer-rate> eta <asb-transfer-eta>")));
}

export function normalizeLegacyValue(value) {
  value = normalizeFunctionRenderingAddress(value);
  value = normalizeObjectRenderingAddress(value);
  value = normalizeToolSearchOrdering(value);
  value = normalizeLsDiagnosticOrdering(value);
  const hadParentGuidance = value.includes("../") && value.includes("AGENTS.md");
  const cleaned = value.replace(/\n?\.\.\/.*AGENTS\.md\n?/gm, "");
  const withoutGuidance = hadParentGuidance ? cleaned.replace(/\n+$/g, "") : cleaned;
  if (withoutGuidance.includes("Output:\n")) {
    return withoutGuidance.replace(/(Output:\n)([\s\S]*?)(?=\nfatal:|$)/g, (match, prefix, listing) => {
      const marker = "<asb-workspace>\n";
      const markerIndex = listing.indexOf(marker);
      if (markerIndex >= 0) {
        const header = listing.slice(0, markerIndex);
        const lines = listing.slice(markerIndex + marker.length).split("\n");
        if (lines.every((line) => line === "" || line.startsWith("external-workload/"))) {
          return prefix + header + marker + "<asb-workspace-listing>\n";
        }
      }
      return match;
    });
  }
  return normalizePytestTestMapsAddress(normalizePytestCallableAddress(
    normalizePytestElapsed(withoutGuidance),
  ));
}

export function normalizeLsDiagnosticOrdering(value) {
  // A single `ls` invocation can write directory entries to stdout while a
  // missing operand diagnostic goes to stderr. OpenCode captures both streams
  // in one tool result, but their relative scheduling is not stable. Retain
  // every diagnostic and every listing line; only place these fixed `ls`
  // diagnostics before the corresponding stdout rendering for comparison.
  const lines = value.split("\n");
  const diagnostics = lines.filter((line) => /^ls: cannot access '.+': /.test(line));
  if (diagnostics.length === 0) return value;
  const ordinary = lines.filter((line) => !/^ls: cannot access '.+': /.test(line));
  return [...diagnostics.sort(), ...ordinary].join("\n");
}

export function normalizePytestElapsed(value) {
  // JSON encodes an earlier line feed as the two characters `\\n`, so a
  // word-boundary-only matcher misses a summary in a structured tool value.
  // Retain every count/category; only elapsed rendering is volatile.
  return value.replace(/(^|\\n|\r?\n)((?:\d+ (?:failed|passed|skipped|deselected|errors?),? )+in )\d+(?:\.\d+)?s\b/g, "$1$2<asb-elapsed>");
}

// Python renders a callable's process-local address in its fixed repr.  This
// is terminal presentation noise, not a tool result change.  Keep the exact
// existing callable-rendering rule in one helper so it applies both before
// parsing a request and after structured request strings have been decoded.
export function normalizeFunctionRenderingAddress(value) {
  // Callable names can include a nested `<locals>` segment, so stop only at
  // the fixed ` at 0x...>` suffix rather than at the first closing bracket.
  return value.replace(/((?:<|\\u003c)function [^\n]*? at )0x[0-9a-f]+(>|\\u003e)/gi, "$1<asb-address>$2");
}

// Python object reprs have the same process-local address volatility, but are
// a distinct rendering form from callables.  Keep this intentionally narrow:
// only a complete one-line `<qualified.name object at 0x...>` representation
// is normalized; surrounding output and the object's rendered class remain
// significant.
export function normalizeObjectRenderingAddress(value) {
  return value.replace(/(<[^<>\n]+ object at )0x[0-9a-f]+>/gi, "$1<asb-address>>");
}


export function normalizePytestCallableAddress(value) {
  // pytest's call-runtest hook prints this complete callable repr in its
  // diagnostic output. The pointer is ASLR/process-local rendering; retain
  // the variable, callable identity, and all adjacent output.
  return value.replace(
    /(\bfunc = <function call_runtest_hook\.<locals>\.<lambda> at )0x[0-9a-f]+>/gi,
    "$1<asb-pytest-callable-address>>",
  );
}

export function normalizePytestTestMapsAddress(value) {
  // This exact pytest diagnostic renderer names its TestMaps fixture instance.
  // Only CPython's process-local object address varies; the fixture and every
  // adjacent diagnostic field remain part of the cassette key.
  return value.replace(
    /(\bself = <astropy\.wcs\.tests\.test_wcs\.TestMaps object at )0x[0-9a-f]+>/gi,
    "$1<asb-pytest-testmaps-address>>",
  );
}
