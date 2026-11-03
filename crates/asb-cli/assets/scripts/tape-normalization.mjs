// Preserve legacy workspace keys without retrying path searches at every byte.
export function normalizeWorkspacePaths(body, replacement = "<asb-workspace>") {
  const runs = /\/?[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)*/g;
  const guestSnapshot = /^\/?asb-snapshot-storage\/snapshot\/filesystem\/workspace(?:\/|$)/;
  return body
    .replace(runs, (run) => guestSnapshot.test(run) ? run : run.replace(/^\/?(?:[A-Za-z0-9_.-]+\/)+workspace-concurrency-\d+(?:\.asb-agent-runtime)?/, replacement))
    .replace(runs, (run, offset, source) => {
      if (guestSnapshot.test(run)) return run;
      const following = source[offset + run.length];
      const suffix = following === "/" || following === '"' || following === "\\" ? following : "";
      const normalized = (run + suffix).replace(/^\/?(?:[A-Za-z0-9_.-]+\/)+workspace(?=\/|["\\])/, replacement);
      return suffix ? normalized.slice(0, -1) : normalized;
    });
}
