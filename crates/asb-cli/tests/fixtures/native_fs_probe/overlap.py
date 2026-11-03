"""Offline fixture for admission of overlapping recorded tool invocations."""
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
import os
from pathlib import Path
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]
BASE = ROOT / 'target/native-fs-probe'
BASE.mkdir(parents=True, exist_ok=True)
spec = importlib.util.spec_from_file_location('native_fs_replay', HERE / 'replay.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

with tempfile.TemporaryDirectory(dir=BASE) as tmp:
    root = Path(tmp)
    evidence = root / 'evidence'
    evidence.mkdir()
    (evidence / 'data').write_bytes(b'from tool b')
    events = [
        {'op': 'marker', 'path': None, 'marker': 'tool-begin:a'},
        {'op': 'marker', 'path': None, 'marker': 'tool-begin:b'},
        {'op': 'create', 'path': '/b.txt', 'handle': 1, 'flags': os.O_WRONLY, 'mode': 0o600},
        {'op': 'write', 'path': '/b.txt', 'handle': 1, 'offset': 0, 'object': 'data'},
        {'op': 'release', 'path': '/b.txt', 'handle': 1},
        {'op': 'marker', 'path': None, 'marker': 'tool-end:a'},
        {'op': 'marker', 'path': None, 'marker': 'tool-end:b'},
        {'op': 'marker', 'path': None, 'marker': 'tool-begin:a'},
        {'op': 'create', 'path': '/second-a.txt', 'handle': 2, 'flags': os.O_WRONLY, 'mode': 0o600},
        {'op': 'write', 'path': '/second-a.txt', 'handle': 2, 'offset': 0, 'object': 'data'},
        {'op': 'release', 'path': '/second-a.txt', 'handle': 2},
        {'op': 'marker', 'path': None, 'marker': 'tool-end:a'},
    ]
    with (evidence / 'events.jsonl').open('w') as output:
        for sequence, event in enumerate(events, 1):
            output.write(json.dumps({'sequence': sequence, **event}) + '\n')

    def replica(index):
        destination = root / f'replay-{index}'
        replay = module.Replay(evidence, destination, enforce_admission=True)
        try:
            replay.admit('a')
            assert replay.apply_until(6) is False
            assert replay.sequence == 1
            assert not (destination / 'b.txt').exists()
            try:
                replay.finish()
            except RuntimeError as error:
                assert 'waiting' in str(error)
            else:
                raise AssertionError('unadmitted tool was silently accepted')
            replay.admit('b')
            assert replay.apply_until(6) is True
            assert (destination / 'b.txt').read_bytes() == b'from tool b'
            try:
                replay.finish()
            except RuntimeError as error:
                assert 'waiting' in str(error)
            else:
                raise AssertionError('repeated invocation reused a prior admission')
            assert not (destination / 'second-a.txt').exists()
            replay.admit('a')
            replay.finish()
            assert (destination / 'second-a.txt').read_bytes() == b'from tool b'
        finally:
            replay.close()

    with ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(replica, range(4)))
    print('Four replicas waited for the overlapping tool before applying its effects')
