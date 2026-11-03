"""Hermes keeps its normal catalogue and marks untaped calls honestly."""
import os
from pathlib import Path
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / 'crates/asb-cli/assets/scripts/hermes-native-tools.py'


class CoverageTest(unittest.TestCase):
    def test_unsupported_calls_execute_only_when_recording_and_invalidate_tape(self):
        with tempfile.TemporaryDirectory() as directory:
            os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(Path(directory) / 'tape')
            os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(Path(directory) / 'progress')
            source = HOOK.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
            namespace = {'__name__': 'hermes_catalogue_fixture'}
            exec(compile(source, str(HOOK), 'exec'), namespace)
            class Registry:
                def __init__(self): self.calls = []
                def dispatch(self, name, args, **kwargs):
                    self.calls.append((name, args, kwargs)); return 'original result'
            class Transport:
                def __init__(self): self.lock = threading.RLock(); self.failed = False
            registry = Registry(); transport = Transport()
            covered = namespace['install_dispatch_coverage'](registry, transport, 'record')
            covered.add('terminal')
            self.assertEqual(registry.dispatch('terminal', {}), 'original result')
            self.assertFalse(transport.failed)
            self.assertEqual(registry.dispatch('file_editor', {'key': 1}), 'original result')
            self.assertTrue(transport.failed)
            self.assertEqual(len(registry.calls), 2)
            registry = Registry(); transport = Transport()
            covered = namespace['install_dispatch_coverage'](registry, transport, 'replay')
            covered.add('terminal')
            self.assertEqual(registry.dispatch('terminal', {}), 'original result')
            with self.assertRaisesRegex(RuntimeError, 'no recorded replay transport'):
                registry.dispatch('file_editor', {})
            self.assertTrue(transport.failed)
            self.assertEqual(len(registry.calls), 1)

if __name__ == '__main__': unittest.main()
