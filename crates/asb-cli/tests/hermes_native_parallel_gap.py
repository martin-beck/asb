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
    recorder.close()
    assert not failures, failures
    assert json.loads((directory / 'progress.json').read_text())['complete']
    rows = [json.loads(line) for line in (directory / 'native.jsonl').read_text().splitlines()]
    requests = [json.loads(base64.b64decode(row['request_b64']))['command']
                for row in rows if row['event'] == 'write']
    assert requests == ['0', '1']
    replay = namespace['Replayer']()
    for request in requests:
        assert replay.invoke(None, request, False, None, None, None,
                             False, '.', False, False, None) == 'ok'
    replay.close()
    assert json.loads((directory / 'progress.json').read_text())['complete']
    reversed_replay = namespace['Replayer']()
    response = reversed_replay.invoke(None, '1', False, None, None, None,
                                      False, '.', False, False, None)
    assert json.loads(response)['asb_native_mismatch'] == 'command'
    reversed_replay.close()
    assert not json.loads((directory / 'progress.json').read_text())['complete']
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
