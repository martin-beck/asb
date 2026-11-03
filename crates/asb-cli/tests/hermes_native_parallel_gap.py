"""Overlapping recording calls must produce a readable sidecar.

This checks framing and sequential response lookup, not full parallel replay
ordering or filesystem-effect reproduction.
"""
import base64
import json
import os
from pathlib import Path
import tempfile
import threading

root = Path(__file__).resolve().parents[3]
base = root / 'target/hermes-parallel-diagnostic'
base.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(dir=base) as temp:
    directory = Path(temp)
    os.environ.update(ASB_HERMES_NATIVE_MODE='record',
                      ASB_HERMES_NATIVE_SIDECAR=str(directory / 'native.jsonl'),
                      ASB_HERMES_NATIVE_PROGRESS=str(directory / 'progress.json'))
    hook = root / 'crates/asb-cli/assets/scripts/hermes-native-tools.py'
    source = hook.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
    namespace = {'__name__': 'hermes_parallel_diagnostic'}
    exec(compile(source, str(hook), 'exec'), namespace)
    recorder = namespace['Recorder']()
    barrier = threading.Barrier(2)
    first_started = threading.Event()
    second_finished = threading.Event()
    failures = []

    def invoke(index):
        try:
            def original(*args, **kwargs):
                if index == 0:
                    first_started.set()
                barrier.wait(timeout=5)
                if index == 0:
                    assert second_finished.wait(timeout=5)
                return 'ok'
            recorder.invoke(original, str(index), False, None, None, None,
                            False, '.', False, False, None)
            if index == 1:
                second_finished.set()
        except Exception as error:
            failures.append(error)

    threads = [threading.Thread(target=invoke, args=(index,)) for index in range(2)]
    threads[0].start()
    assert first_started.wait(timeout=5)
    threads[1].start()
    for thread in threads:
        thread.join()
    recorder.invoke(lambda *args: 'later', '2', False, None, None, None,
                    False, '.', False, False, None)
    recorder.close()
    assert not failures, failures
    assert json.loads((directory / 'progress.json').read_text())['complete']
    if os.environ.get('ASB_TEST_CAPTURE'):
        Path(os.environ['ASB_TEST_CAPTURE']).write_bytes((directory / 'native.jsonl').read_bytes())
    rows = [json.loads(line) for line in (directory / 'native.jsonl').read_text().splitlines()]
    requests = [json.loads(base64.b64decode(row['request_b64']))['command']
                for row in rows if row['event'] == 'write']
    assert requests == ['0', '1', '2']
    replay = namespace['Replayer']()
    for request in requests[:2]:
        assert replay.invoke(None, request, False, None, None, None,
                             False, '.', False, False, None) == 'ok'
    assert replay.invoke(None, '2', False, None, None, None,
                         False, '.', False, False, None) == 'later'
    replay.close()
    assert json.loads((directory / 'progress.json').read_text())['complete']
    reversed_replay = namespace['Replayer']()
    for request in ('1', '0'):
        assert reversed_replay.invoke(None, request, False, None, None, None,
                                      False, '.', False, False, None) == 'ok'
    assert reversed_replay.invoke(None, '2', False, None, None, None,
                                  False, '.', False, False, None) == 'later'
    reversed_replay.close()
    assert json.loads((directory / 'progress.json').read_text())['complete']
    wrong_order = namespace['Replayer']()
    response = wrong_order.invoke(None, '2', False, None, None, None,
                                  False, '.', False, False, None)
    assert json.loads(response)['asb_native_mismatch'] == 'sequence'
    wrong_order.close()
    assert not json.loads((directory / 'progress.json').read_text())['complete']
    rows = [json.loads(line) for line in (directory / 'native.jsonl').read_text().splitlines()]
    by_alias = {}
    for row in rows:
        if row.get('event') in ('poll', 'write', 'output'):
            by_alias.setdefault(row['alias'], []).append(dict(row, alias='terminal-1'))
    legacy_rows = [rows[0], {'event':'session_alias','alias':'terminal-1','session':'launch-1'}]
    for alias in ('terminal-1', 'terminal-2', 'terminal-3'):
        legacy_rows.extend(by_alias[alias])
    legacy_rows.extend([{'event':'exit','alias':'terminal-1','success':True}, rows[-1]])
    legacy = directory / 'legacy.jsonl'
    legacy.write_text(''.join(json.dumps(row) + '\n' for row in legacy_rows))
    namespace['SIDECAR'] = legacy
    legacy_replay = namespace['Replayer']()
    for request, expected in [('0', 'ok'), ('1', 'ok'), ('2', 'later')]:
        assert legacy_replay.invoke(None, request, False, None, None, None,
                                    False, '.', False, False, None) == expected
    legacy_replay.close()
    assert json.loads((directory / 'progress.json').read_text())['complete']
    next(row for row in rows if row['event'] == 'output')['response_sha256'] = '0' * 64
    corrupt = directory / 'corrupt.jsonl'
    corrupt.write_text(''.join(json.dumps(row) + '\n' for row in rows))
    namespace['SIDECAR'] = corrupt
    try:
        namespace['Replayer']()
    except RuntimeError as error:
        assert 'digest' in str(error)
    else:
        raise AssertionError('mutated parallel sidecar was accepted')
    namespace['SIDECAR'] = directory / 'failed.jsonl'
    namespace['PROGRESS'] = directory / 'failed-progress.json'
    failing = namespace['Recorder']()
    def fail(*args, **kwargs):
        raise ValueError('fixture failure')
    try:
        failing.invoke(fail, 'fail', False, None, None, None,
                       False, '.', False, False, None)
    except ValueError:
        pass
    else:
        raise AssertionError('failed tool call was accepted')
    failing.close()
    assert not json.loads((directory / 'failed-progress.json').read_text())['complete']
    print('Overlapping calls record a readable sidecar without serializing execution')
