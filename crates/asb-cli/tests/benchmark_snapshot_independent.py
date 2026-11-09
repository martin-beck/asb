"""Offline ownership and lifecycle tests for separate snapshot custody."""
from pathlib import Path
import sys
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1]/'assets/benchmarks'))
import asb_snapshot_independent as subject


class Input:
    def __init__(self): self.writes=[];self.closed=False
    def write(self,value): self.writes.append(value)
    def flush(self): pass
    def close(self): self.closed=True


class Process:
    def __init__(self,output): self.stdin=Input();self.output=output;self.returncode=None
    def poll(self): return self.returncode
    def wait(self,timeout=None): self.returncode=0;return 0
    def kill(self): self.returncode=-9
    def communicate(self,value,timeout=None):
        self.stdin.write(value);self.returncode=0;return self.output,None


class IndependentTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        root=Path(self.temp.name);self.prep=Process(b'CLEANED\n');self.side=Process(b'SAVED\n')
        self.container=Mock();self.container.id='task-id'
        self.container.attrs={'State':{'Pid':123,'StartedAt':'first','Running':True},'Image':'sha256:'+'a'*64}
        self.container.exec_run.return_value=SimpleNamespace(exit_code=0,output=b'456\n')
        self.sidecar=Mock();self.sidecar.id='side-id'
        self.client=Mock();self.client.containers.create.return_value=self.sidecar
        self.journal=Mock();self.journal.labels={'asb.owner':'asb'};self.journal.declare.return_value=self.journal.labels
        self.task=SimpleNamespace(artifacts=root,attempt='asb-'+'a'*32,client=self.client,journal=self.journal,
                                  workspace=SimpleNamespace(container=self.container,snapshot_storage=root))
        self.tools={'btrfs':'btrfs','mkfs.btrfs':'mkfs','rsync':'rsync'}
        for name,kwargs in [('upload',{'return_value':None}),('_line',{'side_effect':[b'PREPARED\n',b'MOUNT:456\n',b'READY\n']})]:
            p=patch.object(subject,name,**kwargs);p.start();self.addCleanup(p.stop)
        p=patch.object(subject.subprocess,'Popen',side_effect=[self.prep,self.side]);p.start();self.addCleanup(p.stop)
        self.owned=patch.object(subject,'owned_resource',return_value=self.sidecar);self.lookup=self.owned.start();self.addCleanup(self.owned.stop)

    def start(self):
        return subject.IndependentSnapshot(self.task,self.tools,None,['docker'],9999999999,replay=True)

    def test_normal_settles_before_preparer_and_removes_owned_sidecar(self):
        owner=self.start();owner.close()
        self.assertEqual(self.side.stdin.writes,[b'START\n',b'STOP\n'])
        self.assertEqual(self.prep.stdin.writes,[b'CLEANUP\n'])
        self.sidecar.remove.assert_called_once_with(force=False)
        self.journal.declare.assert_called_once_with('containers',self.task.attempt+'-snapshot-custody')

    def test_cpu_allocation_is_applied_and_verified_before_start(self):
        self.task.workspace.cpu_set = '1,3'
        self.sidecar.exec_run.return_value = SimpleNamespace(exit_code=0, output=b'Cpus_allowed_list:\t1,3\n')
        owner = self.start()
        self.assertEqual(self.client.containers.create.call_args.kwargs['cpuset_cpus'], '1,3')
        self.sidecar.exec_run.assert_called_once()
        self.assertEqual(self.side.stdin.writes, [b'START\n'])
        owner.close()

    def test_cpu_allocation_mismatch_aborts_before_start(self):
        self.task.workspace.cpu_set = '1,3'
        self.sidecar.exec_run.return_value = SimpleNamespace(exit_code=0, output=b'Cpus_allowed_list:\t1\n')
        with self.assertRaisesRegex(subject.ExecutionError, 'differs from the requested set'):
            self.start()
        self.assertEqual(self.side.stdin.writes, [])
        self.assertTrue(self.side.stdin.closed and self.prep.stdin.closed)

    def test_wrong_mount_fails_before_start_and_closes_both_transports(self):
        subject._line.side_effect=[b'PREPARED\n',b'MOUNT:999\n']
        with self.assertRaisesRegex(subject.ExecutionError,'wrong mount namespace'):self.start()
        self.assertEqual(self.side.stdin.writes,[])
        self.assertTrue(self.side.stdin.closed and self.prep.stdin.closed)
        self.sidecar.remove.assert_not_called()

    def test_changed_task_identity_fails_before_start(self):
        def reload_container():
            if self.container.reload.call_count == 2:
                self.container.attrs['State']['Pid'] = 124
        self.container.reload.side_effect = reload_container
        with self.assertRaisesRegex(subject.ExecutionError, 'changed before custody'):
            self.start()
        self.assertEqual(self.side.stdin.writes, [])
        self.assertTrue(self.side.stdin.closed and self.prep.stdin.closed)

    def test_unavailable_mount_identity_never_creates_sidecar(self):
        self.container.exec_run.return_value = SimpleNamespace(exit_code=1, output=b'')
        with self.assertRaisesRegex(subject.ExecutionError, 'mount identity is unavailable'):
            self.start()
        self.client.containers.create.assert_not_called()
        self.assertTrue(self.prep.stdin.closed)

    def test_interruption_without_confirmed_reset_invalidates_without_forensic(self):
        owner=self.start();owner.interrupt()
        self.assertEqual(self.side.stdin.writes,[b'START\n'])
        self.assertTrue(self.side.stdin.closed and self.prep.stdin.closed)
        self.sidecar.remove.assert_not_called()

    def test_confirmed_reset_requests_only_incomplete_forensic_capture(self):
        owner=self.start();self.container.attrs['State']={'Pid':124,'StartedAt':'second','Running':True}
        self.side.output=b'FORENSIC_SAVED_INCOMPLETE\n';owner.interrupt()
        self.assertEqual(self.side.stdin.writes,[b'START\n',b'FORENSIC\n'])
        self.assertEqual(self.prep.stdin.writes,[])
        self.assertEqual(self.prep.poll(),0)
        self.sidecar.remove.assert_called_once_with(force=False)

    def test_reset_reaps_stuck_preparer_after_timeout(self):
        owner=self.start();self.container.attrs['State']={'Pid':0,'StartedAt':'first','Running':False}
        self.side.output=b'FORENSIC_SAVED_INCOMPLETE\n'
        self.prep.wait = Mock(side_effect=[subprocess.TimeoutExpired('docker exec', 60), -9])
        self.prep.kill = Mock(wraps=self.prep.kill)
        owner.interrupt()
        self.prep.kill.assert_called_once_with()
        self.assertEqual(self.prep.wait.call_count, 2)

    def test_unknown_task_state_does_not_authorize_forensic_capture(self):
        owner=self.start();self.container.attrs['State']={'Running':None}
        with self.assertRaisesRegex(subject.ExecutionError, 'reset cannot be confirmed'):
            owner.interrupt()
        self.assertEqual(self.side.stdin.writes,[b'START\n'])
        self.assertTrue(self.side.stdin.closed and self.prep.stdin.closed)
        self.sidecar.remove.assert_not_called()

    def test_abort_reaps_both_children_when_one_pipe_is_broken(self):
        owner=self.start()
        self.side.stdin.close=Mock(side_effect=BrokenPipeError())
        owner.abort()
        self.assertEqual(self.side.poll(),0)
        self.assertEqual(self.prep.poll(),0)
        self.assertTrue(self.prep.stdin.closed)

    def test_failed_custodian_settlement_aborts_preparer(self):
        owner=self.start();self.side.output=b'NOT_SAVED\n'
        with self.assertRaisesRegex(subject.ExecutionError, 'custodian did not settle'):
            owner.close()
        self.assertTrue(self.prep.stdin.closed)
        self.assertEqual(self.prep.poll(),0)
        self.sidecar.remove.assert_not_called()

    def test_preparer_timeout_is_preserved_and_reaped(self):
        owner=self.start()
        self.prep.communicate=Mock(side_effect=subprocess.TimeoutExpired('docker exec',600))
        with self.assertRaises(subprocess.TimeoutExpired):
            owner.close()
        self.assertTrue(self.prep.stdin.closed)
        self.assertEqual(self.prep.poll(),0)
        self.sidecar.remove.assert_not_called()

    def test_preparer_rejects_success_marker_embedded_in_an_error(self):
        owner=self.start();self.prep.output=b'NOT_CLEANED\n'
        with self.assertRaisesRegex(subject.ExecutionError, 'preparer did not settle'):
            owner.close()
        self.sidecar.remove.assert_not_called()

    def test_failed_task_inspection_aborts_both_clients(self):
        owner=self.start();self.container.reload.side_effect=RuntimeError('inspection failed')
        with self.assertRaisesRegex(RuntimeError, 'inspection failed'):
            owner.interrupt()
        self.assertTrue(self.side.stdin.closed and self.prep.stdin.closed)
        self.assertEqual(self.side.poll(),0)
        self.assertEqual(self.prep.poll(),0)
        self.sidecar.remove.assert_not_called()

    def test_startup_error_survives_cleanup_error_and_other_client_is_reaped(self):
        subject._line.side_effect=[b'PREPARED\n',b'MOUNT:999\n']
        self.side.stdin.close=Mock(side_effect=OSError('pipe failure'))
        with self.assertRaisesRegex(subject.ExecutionError, 'wrong mount namespace') as caught:
            self.start()
        self.assertTrue(any('cleanup also failed: OSError' in note for note in caught.exception.__notes__))
        self.assertTrue(self.prep.stdin.closed)
        self.assertIsNotNone(self.side.poll())
        self.assertEqual(self.prep.poll(),0)


if __name__=='__main__':unittest.main()
