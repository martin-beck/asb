"""Hermes keeps its normal catalogue and marks untaped calls honestly."""
import os
import json
import base64
import contextvars
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

    def test_untaped_parent_links_recorded_native_child_across_context_thread(self):
        with tempfile.TemporaryDirectory() as directory:
            os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(Path(directory) / 'native')
            os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(Path(directory) / 'progress')
            source = HOOK.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
            namespace = {'__name__': 'hermes_nested_lineage_fixture'}
            exec(compile(source, str(HOOK), 'exec'), namespace)
            markers = []
            namespace['_filesystem_marker'] = lambda action, alias: markers.append((action, alias)) or True
            capture = namespace['UntapedToolCapture'](Path(directory) / 'untaped-tools.jsonl')
            native = namespace['Recorder']()
            def parent(_name, _args, **_kwargs):
                ctx = contextvars.copy_context()
                errors = []
                def child():
                    try:
                        ctx.run(native.invoke, lambda *_: 'child-result', 'pwd', False, None,
                                None, None, False, '.', False, False, None)
                    except BaseException as error: errors.append(error)
                worker = threading.Thread(target=child); worker.start(); worker.join(5)
                self.assertFalse(worker.is_alive()); self.assertEqual(errors, [])
                return 'parent-result'
            self.assertEqual(capture.invoke(parent, 'execute_code', {'code': 'fixture'}, {}), 'parent-result')
            native.close(); capture.close()
            rows = [json.loads(line) for line in (Path(directory) / 'native').read_text().splitlines()]
            request = next(json.loads(base64.b64decode(row['request_b64'])) for row in rows if row['event'] == 'write')
            self.assertEqual(request['parent_alias'], 'untaped-1')
            self.assertEqual(markers, [('begin', 'untaped-1'), ('begin', 'terminal-1'),
                                       ('end', 'terminal-1'), ('end', 'untaped-1')])
            self.assertIsNone(namespace['PARENT_CALL'].get())

    def test_http_drain_waits_for_inflight_request_and_unwinds_exceptions(self):
        with tempfile.TemporaryDirectory() as directory:
            os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(Path(directory) / 'tape')
            os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(Path(directory) / 'progress')
            source = HOOK.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
            namespace = {'__name__': 'hermes_http_drain_fixture'}
            exec(compile(source, str(HOOK), 'exec'), namespace)
            started = threading.Event(); release = threading.Event()
            class Client:
                def send(self, error=False):
                    started.set(); assert release.wait(5)
                    if error: raise ValueError('fixture')
                    return 'response'
            drain = namespace['SynchronousHttpDrain'](); drain.install(Client)
            result = []
            worker = threading.Thread(target=lambda: result.append(Client().send()))
            worker.start(); self.assertTrue(started.wait(5))
            self.assertFalse(drain.wait(.01))
            release.set(); self.assertTrue(drain.wait(5)); worker.join(5)
            self.assertEqual(result, ['response'])
            with self.assertRaises(ValueError): Client().send(error=True)
            self.assertTrue(drain.wait(.01))

    def test_call_id_diagnostic_does_not_ignore_other_request_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(Path(directory) / 'tape')
            os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(Path(directory) / 'progress')
            source = HOOK.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
            namespace = {'__name__': 'hermes_identity_fixture'}
            exec(compile(source, str(HOOK), 'exec'), namespace)
            check = namespace['_differs_only_in_tool_call_id']
            request = {'name':'read_file', 'arguments':{'path':'same'}, 'tool_call_id':'a'}
            self.assertTrue(check(json.dumps(request), json.dumps({**request, 'tool_call_id':'b'})))
            self.assertFalse(check(json.dumps(request), json.dumps({**request, 'arguments':{'path':'other'}, 'tool_call_id':'b'})))
            self.assertFalse(check(json.dumps(request), json.dumps(request)))
            self.assertFalse(check('not-json', '{}'))

if __name__ == '__main__': unittest.main()
