"""Offline diagnostic: overlapping Hermes calls must remain replayable.

The current transport marks an interleaved stream complete even though its
reader rejects that stream. This is a defect diagnostic, not a passing
qualification of parallel replay. A repair must preserve concurrent execution.
"""
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
    failures = []

    def invoke(index):
        try:
            def original(*args, **kwargs):
                barrier.wait(timeout=5)
                return 'ok'
            recorder.invoke(original, str(index), False, None, None, None,
                            False, '.', False, False, None)
        except Exception as error:
            failures.append(error)

    threads = [threading.Thread(target=invoke, args=(index,)) for index in range(2)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    recorder.close()
    assert not failures, failures
    assert json.loads((directory / 'progress.json').read_text())['complete']
    try:
        namespace['Replayer']()
    except RuntimeError as error:
        assert str(error) == 'Hermes native replay sidecar is invalid'
    else:
        raise AssertionError('parallel sidecar became readable; update diagnostic to verify replay')
    print('Confirmed: overlapping Hermes calls produce a completed but unreadable sidecar')
