"""Experimental classification of recorded filesystem operations.

This is evidence analysis only. It does not enforce a boundary or replay I/O.
"""


def _positive(value):
    return type(value) is int and value > 0


def _origin(origin):
    if not isinstance(origin, dict):
        return None
    values = tuple(origin.get(key) for key in
                   ('pid', 'tid', 'process_start_ticks', 'thread_start_ticks'))
    return values if all(_positive(value) for value in values) else None


def _caller(caller):
    if (not isinstance(caller, dict) or caller.get('ancestry_complete') is not True
            or caller.get('thread_ancestry_complete') is not True):
        return None
    tid, tgid, thread_start = (caller.get(key) for key in ('tid', 'tgid', 'start_ticks'))
    ancestry = caller.get('ancestry')
    if not all(_positive(value) for value in (tid, tgid, thread_start)):
        return None
    if not isinstance(ancestry, list) or not ancestry or len(ancestry) > 64:
        return None
    for index, entry in enumerate(ancestry):
        if (not isinstance(entry, dict) or not _positive(entry.get('tgid'))
                or not _positive(entry.get('start_ticks'))):
            return None
        if index < len(ancestry) - 1:
            creator = entry.get('creator_thread')
            if (not isinstance(creator, dict) or not _positive(creator.get('tid'))
                    or not _positive(creator.get('start_ticks'))):
                return None
    if ancestry[0]['tgid'] != tgid:
        return None
    return tid, tgid, thread_start, ancestry


def _belongs(caller, origin):
    tid, tgid, thread_start, ancestry = caller
    pid, marker_tid, process_start, marker_thread_start = origin
    if (tgid == pid and tid == marker_tid and thread_start == marker_thread_start
            and ancestry[0]['start_ticks'] == process_start):
        return True
    for index in range(1, len(ancestry)):
        ancestor = ancestry[index]
        creator = ancestry[index - 1]['creator_thread']
        if (ancestor['tgid'] == pid and ancestor['start_ticks'] == process_start
                and creator['tid'] == marker_tid and creator['start_ticks'] == marker_thread_start):
            return True
    return False


class Attribution:
    """Classify against all active markers, preserving uncertainty explicitly."""
    def __init__(self):
        self.active = {}
        self.handles = {}
        self.sequence = 0

    def observe(self, event):
        sequence = event.get('sequence')
        if type(sequence) is not int or sequence != self.sequence + 1:
            raise ValueError('filesystem event sequence has a gap')
        self.sequence = sequence
        if event.get('op') == 'marker':
            marker = event.get('marker')
            if not isinstance(marker, str):
                raise ValueError('filesystem marker is invalid')
            if marker.startswith('tool-begin:'):
                alias = marker.removeprefix('tool-begin:')
                if not alias or alias in self.active:
                    raise ValueError('filesystem tool begin is duplicate or invalid')
                self.active[alias] = _origin(event.get('origin'))
            elif marker.startswith('tool-end:'):
                alias = marker.removeprefix('tool-end:')
                if alias not in self.active:
                    raise ValueError('filesystem tool end has no active begin')
                del self.active[alias]
            else:
                raise ValueError('filesystem marker is unsupported')
            return {'kind': 'marker'}
        op = event.get('op')
        handle = event.get('handle')
        if op == 'release':
            opened_as = self.handles.pop(handle, None)
            if opened_as is None:
                return {'kind': 'unknown'}
            # FUSE release may have no caller. The opener is evidence about
            # handle lifetime, not a claim about which thread closed it.
            return {'kind': 'handle-release', 'handle': handle, 'opened_as': opened_as}
        caller = _caller(event.get('caller'))
        if caller is None or any(origin is None for origin in self.active.values()):
            scope = {'kind': 'unknown'}
        else:
            owners = [alias for alias, origin in self.active.items() if _belongs(caller, origin)]
            if len(owners) == 1:
                scope = {'kind': 'tool', 'alias': owners[0]}
            elif len(owners) > 1:
                scope = {'kind': 'ambiguous', 'aliases': sorted(owners)}
            else:
                scope = {'kind': 'direct'}
        if op in ('open', 'create') and _positive(handle):
            if handle in self.handles:
                raise ValueError('filesystem handle is already open')
            self.handles[handle] = scope
        elif _positive(handle) and handle in self.handles:
            opened_as = self.handles[handle]
            if (opened_as.get('kind') in ('direct', 'tool')
                    and scope.get('kind') in ('direct', 'tool') and opened_as != scope):
                return {'kind': 'shared-handle', 'handle': handle,
                        'opened_as': opened_as, 'used_as': scope}
        return scope

    def finish(self):
        if self.active:
            raise ValueError('filesystem tool markers did not settle')
