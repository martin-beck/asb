// Preserve fetch behavior while making otherwise hidden transport failures
// diagnosable. Never print request data, URLs, exception messages or secrets.
export function installFetchDiagnostic(target = globalThis, report = text => process.stderr.write(text)) {
  if (typeof target.fetch !== 'function') return;
  const original = target.fetch;
  const known = new Set(['ECONNREFUSED', 'ECONNRESET', 'ETIMEDOUT', 'EPIPE', 'ENOTFOUND', 'EHOSTUNREACH', 'ENETUNREACH', 'UND_ERR_CONNECT_TIMEOUT', 'UND_ERR_SOCKET']);
  let reports = 0;
  target.fetch = async function (...args) {
    try {
      return await Reflect.apply(original, this, args);
    } catch (error) {
      if (reports++ < 8) {
        let code;
        try { code = error?.cause?.code ?? error?.code; } catch { /* unknown cause */ }
        try { report(`ASB Pi fetch transport failure: ${known.has(code) ? code : 'unknown'}\n`); } catch { /* never mask the original error */ }
      }
      throw error;
    }
  };
}
