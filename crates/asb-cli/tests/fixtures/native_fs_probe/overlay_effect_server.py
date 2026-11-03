"""Test-only per-replica operation cursor inside an isolated overlay."""
import json
import os
from pathlib import Path
import socket
import sys
from replay import Replay

evidence = Path('/work/evidence')
index = sys.argv[1]
ends = {event['marker']: event['sequence'] for event in
        (json.loads(line) for line in (evidence / 'events.jsonl').read_text().splitlines())
        if event['op'] == 'marker'}
sock_path = Path(f'/work/control/effect-{index}.sock')
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(str(sock_path)); listener.listen(1); listener.settimeout(60)
cursor = Replay(evidence, Path('/work/mount'), enforce_admission=True, existing=True)
try:
    connection, _ = listener.accept()
    with connection, connection.makefile('rwb', buffering=0) as stream:
        if stream.readline().decode().strip() != 'tool-end:one':
            raise RuntimeError('unexpected recorded tool boundary')
        cursor.admit('one')
        if not cursor.apply_until(ends['tool-end:one']):
            raise RuntimeError('unadmitted operation before tool end')
        stream.write((str(cursor.sequence) + '\n').encode())
    cursor.finish()
finally:
    cursor.close(); listener.close(); sock_path.unlink(missing_ok=True)
