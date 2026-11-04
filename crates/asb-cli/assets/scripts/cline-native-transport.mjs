// Cline owns the mapping between its native hooks and ASB's shared broker.
import {brokerRequest} from './native-tool-broker.mjs';
import {snapshotBoundary} from './native-snapshot-boundary.mjs';

export function createClineNativeTransport(socket, marker) {
  if (typeof socket !== 'string' || !socket || typeof marker !== 'string' || !marker)
    throw new TypeError('Cline native transport requires broker and snapshot sockets');
  return {
    async start(request) { return (await brokerRequest(socket, {op: 'start', request})).token; },
    async begin(_request, token) { await snapshotBoundary(marker, 'record', 'begin', token, 'cline'); },
    async end(token) { await snapshotBoundary(marker, 'record', 'end', token, 'cline'); },
    async finish(token, value) { await brokerRequest(socket, {op: 'finish', token, outcome: {ok: true, value}}); },
    async invalidate(token) { await brokerRequest(socket, {op: 'invalidate', token}); },
    async invoke(request) { return (await brokerRequest(socket, {op: 'invoke', request})).value; },
  };
}
