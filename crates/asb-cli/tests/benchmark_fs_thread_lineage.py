"""Linux creator-thread evidence for concurrent agent/tool subprocesses."""
import os
from pathlib import Path
import subprocess
import sys
import threading
import types
import unittest

ROOT = Path(__file__).resolve().parents[3]
source = (ROOT/'crates/asb-cli/assets/benchmarks/asb_fs_proxy.py').read_text().split('\nclass Proxy(Operations):', 1)[0]
fuse = types.ModuleType('fuse')
fuse.FUSE = lambda *_args, **_kwargs: None
fuse.FuseOSError = OSError
fuse.Operations = type('Operations', (), {})
fuse.fuse_get_context = lambda: (0, 0, 0)
sys.modules['fuse'] = fuse
old_argv = sys.argv; sys.argv = ['fixture', '/tmp/backing', '/tmp/mount', '/tmp/evidence']
module = {'__name__':'fs_lineage_fixture'}
try:
    exec(compile(source, 'asb_fs_proxy.py', 'exec'), module)
finally:
    sys.argv = old_argv

@unittest.skipUnless(sys.platform.startswith('linux'), 'Linux procfs is required')
class ThreadLineageTest(unittest.TestCase):
    def test_sibling_processes_bind_to_their_actual_creator_threads(self):
        os.environ['ASB_PROBE_ROOT_TGID'] = str(os.getpid())
        ready = threading.Barrier(3); release = threading.Event(); children = []
        def worker():
            process = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(10)'])
            try:
                children.append((process.pid, threading.get_native_id()))
                ready.wait(timeout=5); release.wait(5)
            finally:
                process.terminate(); process.wait(timeout=5)
        threads = [threading.Thread(target=worker) for _ in range(2)]
        for thread in threads: thread.start()
        try:
            ready.wait(timeout=5)
            self.assertEqual(len(children), 2)
            for pid, creator_tid in children:
                identity = module['caller_identity'](pid)
                self.assertTrue(identity['ancestry_complete'])
                self.assertTrue(identity['thread_ancestry_complete'])
                self.assertEqual(identity['ancestry'][0]['creator_thread']['tid'], creator_tid)
                self.assertGreater(identity['ancestry'][0]['creator_thread']['start_ticks'], 0)
                self.assertIsNone(module['creator_thread'](pid, os.getpid(), identity['start_ticks'] + 1))
            origin = module['marker_origin'](os.getpid(), threading.get_native_id())
            self.assertIsNotNone(origin['process_start_ticks'])
            self.assertIsNotNone(origin['thread_start_ticks'])
            wrong = module['marker_origin'](os.getpid() + 10000000, threading.get_native_id())
            self.assertIsNone(wrong['process_start_ticks'])
            self.assertIsNone(wrong['thread_start_ticks'])
        finally:
            release.set()
            for thread in threads: thread.join(8)
            self.assertTrue(all(not thread.is_alive() for thread in threads))

    def test_missing_process_is_explicitly_incomplete(self):
        os.environ['ASB_PROBE_ROOT_TGID'] = str(os.getpid())
        result = module['caller_identity'](999999999)
        self.assertFalse(result['ancestry_complete'])
        self.assertFalse(result['thread_ancestry_complete'])
        self.assertEqual(result['ancestry'], [])

if __name__ == '__main__': unittest.main()
