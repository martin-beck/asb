"""Test sidecar request validation before experimental filesystem effects."""
import importlib.util
import os
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]
evidence, baseline, sidecar, destination, end_sequence = sys.argv[1:]
evidence, baseline, sidecar, destination = map(Path, (evidence, baseline, sidecar, destination))
os.environ.update(ASB_HERMES_NATIVE_MODE='replay', ASB_HERMES_NATIVE_SIDECAR=str(sidecar),
                  ASB_HERMES_NATIVE_PROGRESS=str(destination.parent / (destination.name + '-progress.json')))
spec = importlib.util.spec_from_file_location('native_fs_replay', HERE / 'replay.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
hook = ROOT / 'crates/asb-cli/assets/scripts/hermes-native-tools.py'
source = hook.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
namespace = {'__name__': 'hermes_request_fixture'}
exec(compile(source, str(hook), 'exec'), namespace)
replay = module.Replay(evidence, destination, baseline, enforce_admission=True)
coordinator = module.Coordinator(replay)
os.chdir(destination)
args = ('write fixture', False, None, None, None, False, '.', False, False, None)

def original(*_):
    raise AssertionError('replay invoked the original tool')

def validate(transport, request):
    output = transport.invoke(original, *request)
    if transport.failed:
        raise ValueError('Hermes request mismatch')
    return output

try:
    mismatch = namespace['Replayer']()
    try:
        coordinator.invoke('hermes', int(end_sequence), 2,
                           lambda: validate(mismatch, ('different command', *args[1:])))
    except ValueError:
        pass
    else:
        raise AssertionError('wrong Hermes request accepted')
    assert replay.sequence == 0 and not Path('result.txt').exists()
    replay.admit('child')
    transport = namespace['Replayer']()
    response = coordinator.invoke('hermes', int(end_sequence), 2,
                                  lambda: validate(transport, args))
    assert response == 'write succeeded' and Path('result.txt').read_text() == 'recorded contents'
    transport.close()
finally:
    replay.close()
