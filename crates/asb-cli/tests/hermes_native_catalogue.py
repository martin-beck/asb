"""Hermes keeps its normal catalogue and marks untaped calls honestly."""
import os
import json
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
            transport.untaped_capture.close()
            events = [json.loads(line) for line in (Path(directory) / 'untaped-tools.jsonl').read_text().splitlines()]
            self.assertEqual([event['event'] for event in events], ['begin', 'end'])
            self.assertEqual(events[0]['tool'], 'file_editor')
            self.assertEqual(events[0]['arguments'], {'key': 1})
            self.assertEqual(events[1]['result'], 'original result')
            status = json.loads((Path(directory) / 'untaped-tools-status.json').read_text())
            self.assertTrue(status['complete'])
            self.assertEqual(status['calls_started'], 1)
            registry = Registry(); transport = Transport()
            covered = namespace['install_dispatch_coverage'](registry, transport, 'replay')
            covered.add('terminal')
            self.assertEqual(registry.dispatch('terminal', {}), 'original result')
            with self.assertRaisesRegex(RuntimeError, 'no recorded replay transport'):
                registry.dispatch('file_editor', {})
            self.assertTrue(transport.failed)
            self.assertEqual(len(registry.calls), 1)

    def test_capture_limit_does_not_change_tool_result_and_reports_incomplete(self):
        with tempfile.TemporaryDirectory() as directory:
            os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(Path(directory) / 'tape')
            os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(Path(directory) / 'progress')
            source = HOOK.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
            namespace = {'__name__': 'hermes_capture_limit_fixture'}
            exec(compile(source, str(HOOK), 'exec'), namespace)
            capture = namespace['UntapedToolCapture'](Path(directory) / 'untaped-tools.jsonl')
            capture.LIMIT = 8
            self.assertEqual(capture.invoke(lambda *_args, **_kwargs: 'actual', 'read_file', {}, {}), 'actual')
            capture.close()
            status = json.loads((Path(directory) / 'untaped-tools-status.json').read_text())
            self.assertFalse(status['complete'])
            self.assertEqual(status['calls_started'], 1)

    def test_exception_is_recorded_and_propagates_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(Path(directory) / 'tape')
            os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(Path(directory) / 'progress')
            source = HOOK.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
            namespace = {'__name__': 'hermes_capture_exception_fixture'}
            exec(compile(source, str(HOOK), 'exec'), namespace)
            capture = namespace['UntapedToolCapture'](Path(directory) / 'untaped-tools.jsonl')
            error = ValueError('fixture failure')
            def fail(*_args, **_kwargs): raise error
            with self.assertRaises(ValueError) as caught:
                capture.invoke(fail, 'patch', {'path': 'fixture'}, {})
            self.assertIs(caught.exception, error)
            capture.close()
            events = [json.loads(line) for line in (Path(directory) / 'untaped-tools.jsonl').read_text().splitlines()]
            self.assertEqual([e['event'] for e in events], ['begin', 'error'])
            self.assertEqual(events[1]['error_type'], 'ValueError')
            self.assertEqual(events[1]['error'], 'fixture failure')
            status = json.loads((Path(directory) / 'untaped-tools-status.json').read_text())
            self.assertTrue(status['complete'])
            self.assertEqual(status['calls_completed'], 1)

    def test_overlapping_calls_retain_causal_begin_and_end_order(self):
        with tempfile.TemporaryDirectory() as directory:
            os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(Path(directory) / 'tape')
            os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(Path(directory) / 'progress')
            source = HOOK.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
            namespace = {'__name__': 'hermes_capture_parallel_fixture'}
            exec(compile(source, str(HOOK), 'exec'), namespace)
            capture = namespace['UntapedToolCapture'](Path(directory) / 'untaped-tools.jsonl')
            parent_started = threading.Event(); child_done = threading.Event()
            def original(name, _args, **_kwargs):
                if name == 'parent':
                    parent_started.set(); assert child_done.wait(5)
                return name
            def invoke_parent(): capture.invoke(original, 'parent', {}, {})
            worker = threading.Thread(target=invoke_parent); worker.start()
            self.assertTrue(parent_started.wait(5))
            self.assertEqual(capture.invoke(original, 'child', {}, {}), 'child')
            child_done.set(); worker.join(5); self.assertFalse(worker.is_alive())
            capture.close()
            events = [json.loads(line) for line in (Path(directory) / 'untaped-tools.jsonl').read_text().splitlines()]
            self.assertEqual([(e['event'], e['sequence']) for e in events],
                             [('begin', 1), ('begin', 2), ('end', 2), ('end', 1)])
            self.assertEqual(sorted(e['relative_ns'] for e in events), [e['relative_ns'] for e in events])
            status = json.loads((Path(directory) / 'untaped-tools-status.json').read_text())
            self.assertEqual(status['calls_completed'], 2)
            self.assertTrue(status['complete'])

if __name__ == '__main__': unittest.main()
