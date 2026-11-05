"""Session cancellation must not interrupt its own cleanup on a second signal."""
import os
from pathlib import Path
import signal
import subprocess
import sys
import unittest

SOURCE = Path(__file__).parents[1] / 'assets' / 'benchmarks'


class SweBenchSignalTests(unittest.TestCase):
    def test_interrupt_and_termination_are_idempotent_during_cleanup(self):
        program = r'''
import sys, time
sys.path.insert(0, sys.argv[1])
from asb_swebench import install_session_cancel_handlers
install_session_cancel_handlers()
try:
    print('READY', flush=True)
    while True: time.sleep(.05)
except InterruptedError:
    print('CLEANING', flush=True)
    time.sleep(.3)
    print('CLEANED', flush=True)
'''
        for first, second in ((signal.SIGINT, signal.SIGTERM), (signal.SIGTERM, signal.SIGINT)):
            with self.subTest(first=first, second=second):
                child = subprocess.Popen([sys.executable, '-c', program, str(SOURCE)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                try:
                    self.assertEqual(child.stdout.readline().strip(), 'READY')
                    os.kill(child.pid, first)
                    self.assertEqual(child.stdout.readline().strip(), 'CLEANING')
                    os.kill(child.pid, second)
                    stdout, stderr = child.communicate(timeout=5)
                    self.assertEqual(child.returncode, 0, stderr)
                    self.assertEqual(stdout.strip(), 'CLEANED')
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait()


if __name__ == '__main__':
    unittest.main()
