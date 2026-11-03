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
        names = ('write','read','patch','search','terminal') if os.environ.get('ASB_FIXTURE_MIXED') == '1' else ('one',)
        for name in names:
            if stream.readline().decode().strip() != 'tool-end:' + name:
                raise RuntimeError('unexpected recorded tool boundary')
            cursor.admit(name)
            if not cursor.apply_until(ends['tool-end:' + name]):
                raise RuntimeError('unadmitted operation before tool end')
            if len(names) > 1:
                content = Path('/work/mount/asb-real-agent.txt').read_text()
                if content != ('before' if name in ('write','read') else 'fixture'):
                    raise RuntimeError('file state differs at a tool boundary')
            stream.write((str(cursor.sequence) + '\n').encode())
    cursor.finish()
finally:
    cursor.close(); listener.close(); sock_path.unlink(missing_ok=True)
