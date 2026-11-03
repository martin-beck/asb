"""Exercise observed replay startup and teardown without mounting the host."""
import contextlib
import io
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'crates/asb-cli/assets/benchmarks'))
import asb_fs_observed_replay as observed


class ObservedReplayTest(unittest.TestCase):
    def test_uses_backing_workspace_and_finishes_before_unmount(self):
        steps = []
        class Recorder:
            lower = Path('/backing')
            recording = Path('/trace')
            def __init__(self, *args, **kwargs):
                steps.append(('recorder', args, kwargs))
            def __enter__(self):
                steps.append('mount')
                return self
            def __exit__(self, *_):
                steps.append('unmount')
        class Server:
            def __init__(self, *args, **kwargs):
                steps.append(('server', args, kwargs))
            def finish(self):
                steps.append('finish')
        with patch.object(observed, 'FilesystemRecorder', Recorder), patch.object(observed, 'ReplayServer', Server), contextlib.redirect_stdout(io.StringIO()) as output:
            observed.run('/workspace', '/run', '/proxy', '/binding', '/lib', '/evidence', '/socket', io.StringIO('STOP\n'))
        self.assertEqual(output.getvalue(), 'READY\n')
        self.assertEqual(steps[2], ('server', ('/evidence', Path('/backing'), '/socket'), {'direct_trace': Path('/trace')}))
        self.assertEqual(steps[-2:], ['finish', 'unmount'])

    def test_failure_unmounts_and_reports_incomplete_replay(self):
        steps = []
        class Recorder:
            lower = Path('/backing')
            recording = Path('/trace')
            def __init__(self, *args, **kwargs): pass
            def __enter__(self): return self
            def __exit__(self, *_): steps.append('unmount')
        class Server:
            def __init__(self, *args, **kwargs): pass
            def finish(self): steps.append('finish')
        with patch.object(observed, 'FilesystemRecorder', Recorder), patch.object(observed, 'ReplayServer', Server), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, 'control ended unexpectedly'):
                observed.run('/workspace', '/run', '/proxy', '/binding', '/lib', '/evidence', '/socket', io.StringIO(''))
        self.assertEqual(steps, ['finish', 'unmount'])

    def test_server_start_failure_unmounts(self):
        steps = []
        class Recorder:
            lower = Path('/backing')
            recording = Path('/trace')
            def __init__(self, *args, **kwargs): pass
            def __enter__(self): return self
            def __exit__(self, *_): steps.append('unmount')
        class Server:
            def __init__(self, *args, **kwargs): raise RuntimeError('startup failed')
        with patch.object(observed, 'FilesystemRecorder', Recorder), patch.object(observed, 'ReplayServer', Server):
            with self.assertRaisesRegex(RuntimeError, 'startup failed'):
                observed.run('/workspace', '/run', '/proxy', '/binding', '/lib', '/evidence', '/socket', io.StringIO('STOP\n'))
        self.assertEqual(steps, ['unmount'])


if __name__ == '__main__':
    unittest.main()
