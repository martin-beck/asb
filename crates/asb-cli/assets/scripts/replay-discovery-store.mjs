// Project-local, unselected exploratory candidates. The store contains data,
// never executable source; the replay process reconstructs the known template.
import { createHash, randomBytes } from 'node:crypto';
import { closeSync, constants, fstatSync, fsyncSync, linkSync, lstatSync, mkdirSync, openSync, readSync, readdirSync, unlinkSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { temporaryNormalizerSource } from './replay-normalizer-discovery.mjs';

const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const MAX_FILE = 65536;
const MAX_CANDIDATES = 4096;

function readCandidate(path) {
  const fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile() || stat.size > MAX_FILE) throw new Error('invalid candidate file');
    const buffer = Buffer.alloc(MAX_FILE + 1);
    let length = 0;
    while (length < buffer.length) {
      const n = readSync(fd, buffer, length, buffer.length - length, null);
      if (n === 0) break;
      length += n;
    }
    if (length > MAX_FILE) throw new Error('candidate exceeds size limit');
    return buffer.subarray(0, length);
  } finally { closeSync(fd); }
}

export function prepareStore(path) {
  const parent = lstatSync(dirname(path));
  if (!parent.isDirectory() || parent.isSymbolicLink()) throw new Error('exploration store parent is not a directory');
  try { mkdirSync(path, {mode:0o700}); } catch (error) { if (error.code !== 'EEXIST') throw error; }
  const stat = lstatSync(path);
  if (!stat.isDirectory() || stat.isSymbolicLink()) throw new Error('exploration store is not a directory');
  return path;
}

export function loadCandidates(path) {
  const result = [];
  let failures = 0;
  const names = readdirSync(path).filter(name => /^candidate-[0-9a-f]{64}\.json$/.test(name)).sort();
  if (names.length > MAX_CANDIDATES) throw new Error('exploration store exceeds candidate limit');
  for (const name of names) {
    try {
      const bytes = readCandidate(join(path, name));
      if (bytes.length > MAX_FILE || name !== `candidate-${digest(bytes)}.json`) { failures += 1; continue; }
      const data = JSON.parse(bytes.toString('utf8'));
      if (data.schema_version !== 1 || Object.keys(data).sort().join(',') !== 'entries,schema_version') { failures += 1; continue; }
      const source = temporaryNormalizerSource(data.entries);
      if (source) result.push({id:`explore-${digest(bytes).slice(0, 40)}`, source});
      else failures += 1;
    } catch { failures += 1; /* An invalid candidate is not executable authority. */ }
  }
  return {candidates:result, failures};
}

export function saveCandidate(path, entries) {
  if (!temporaryNormalizerSource(entries)) throw new Error('invalid exploratory candidate');
  const bytes = Buffer.from(JSON.stringify({schema_version:1, entries}));
  if (bytes.length > MAX_FILE) throw new Error('exploratory candidate exceeds size limit');
  const final = join(path, `candidate-${digest(bytes)}.json`);
  const names = readdirSync(path).filter(name => /^candidate-[0-9a-f]{64}\.json$/.test(name));
  if (names.length >= MAX_CANDIDATES && !names.includes(`candidate-${digest(bytes)}.json`)) throw new Error('exploration store exceeds candidate limit');
  const temporary = join(path, `.candidate-${process.pid}-${randomBytes(12).toString('hex')}.tmp`);
  let fd;
  try {
    fd = openSync(temporary, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
    writeFileSync(fd, bytes); fsyncSync(fd); closeSync(fd); fd = undefined;
    try { linkSync(temporary, final); } catch (error) {
      if (error.code !== 'EEXIST' || !readCandidate(final).equals(bytes)) throw error;
    }
  } finally {
    if (fd !== undefined) closeSync(fd);
    try { unlinkSync(temporary); } catch { /* No private temp file was created. */ }
  }
  return final;
}
