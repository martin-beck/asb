// Continue's mapping onto the workload-owned snapshot marker protocol.
// The broker ordinal identifies the same native invocation during replay.
import { connect } from 'node:net';
import { readlinkSync } from 'node:fs';

function identity() {
  if (process.platform !== 'linux') return `${process.pid}:${process.pid}`;
  const match = readlinkSync('/proc/thread-self').match(/^(\d+)\/task\/(\d+)$/);
  if (!match) throw new Error('Continue cannot identify the snapshot marker thread');
  return `${match[1]}:${match[2]}`;
}

export function snapshotBoundary(path, mode, action, ordinal) {
  if (!path) return Promise.resolve();
  if (!['record', 'replay'].includes(mode) || !['begin', 'end'].includes(action) ||
      !Number.isSafeInteger(ordinal) || ordinal < 0) throw new TypeError('Continue snapshot boundary is invalid');
  const suffix = mode === 'record' ? `:${identity()}` : '';
  const message = `tool-${action}:continue-${ordinal}${suffix}\n`;
  return new Promise((resolve, reject) => {
    const socket = connect(path);
    let response = '';
    socket.setEncoding('utf8');
    socket.once('connect', () => socket.write(message));
    socket.on('data', chunk => {
      response += chunk;
      if (response.length > 32) { socket.destroy(); reject(new Error('Continue snapshot boundary response is too large')); return; }
      if (response.includes('\n')) {
        const line = response.slice(0, response.indexOf('\n')).trim();
        socket.end();
        if (/^\d+$/.test(line)) resolve();
        else reject(new Error('Continue snapshot boundary was rejected'));
      }
    });
    socket.once('error', error => reject(new Error('Continue snapshot boundary is unavailable', { cause: error })));
    socket.once('end', () => { if (!response.includes('\n')) reject(new Error('Continue snapshot boundary ended early')); });
  });
}

export function recordBoundaries(path) {
  return {
    begin: (_request, ordinal) => snapshotBoundary(path, 'record', 'begin', ordinal),
    end: (_request, ordinal) => snapshotBoundary(path, 'record', 'end', ordinal),
  };
}

export function replayEffects(path) {
  return event => snapshotBoundary(path, 'replay', 'end', event.ordinal);
}
