"""Replay a small recorded operation fixture; not an ASB replay backend."""
import json, os, subprocess, sys
from pathlib import Path

evidence, root = map(Path, sys.argv[1:3])
root.mkdir(parents=True, exist_ok=False)
if len(sys.argv) > 3:
 subprocess.run(['/bin/cp', '-a', str(Path(sys.argv[3])) + '/.', str(root)], check=True)
handles = {}
try:
 with (evidence / 'events.jsonl').open(encoding='utf-8') as events:
  for line in events:
   event = json.loads(line); op = event['op']
   path = root / event['path'].lstrip('/') if event['path'] is not None else None
   if op in ('open','create'):
    flags = event['flags'] | (os.O_CREAT if op == 'create' else 0)
    fd = os.open(path, flags, event.get('mode', 0o600) & 0o7777)
    assert event['handle'] not in handles
    handles[event['handle']] = fd
   elif op == 'release': os.close(handles.pop(event['handle']))
   elif op == 'write':
    fd = handles[event['handle']]; offset = event['offset']
    with (evidence / event['object']).open('rb') as source:
     while chunk := source.read(64 * 1024):
      count = os.pwrite(fd, chunk, offset)
      if count != len(chunk): raise RuntimeError('short recorded write')
      offset += count
   elif op == 'read':
    fd = handles[event['handle']]; offset = event['offset']
    with (evidence / event['object']).open('rb') as expected:
     while chunk := expected.read(64 * 1024):
      if os.pread(fd, len(chunk), offset) != chunk: raise RuntimeError('recorded read differs')
      offset += len(chunk)
   elif op == 'truncate':
    if event['handle'] is None: os.truncate(path, event['size'])
    else: os.ftruncate(handles[event['handle']], event['size'])
   elif op == 'rename': os.rename(path, root / event['destination'].lstrip('/'))
   elif op == 'link': os.link(path, root / event['destination'].lstrip('/'))
   elif op == 'symlink': os.symlink(event['destination'], path)
   elif op == 'unlink': os.unlink(path)
   elif op == 'mkdir': os.mkdir(path, event['mode'] & 0o7777)
   elif op == 'rmdir': os.rmdir(path)
   else: raise RuntimeError('unsupported recorded operation')
 if handles: raise RuntimeError('recorded handles were not closed')
finally:
 for fd in handles.values(): os.close(fd)
