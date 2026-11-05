"""Offline tests for snapshot lifecycle and adapter marker protocol."""
from concurrent.futures import ThreadPoolExecutor
import json
import io
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import tarfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[3]
sys.path.insert(0,str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_snapshot_service import MarkerServer,SnapshotService,SnapshotReplay,SnapshotApplier
import asb_snapshot_service
from asb_snapshot_guest import unpack_streams
import asb_snapshot_guest
from asb_snapshot_control import GuestSnapshot, settle_on_exit

SCRATCH=ROOT/'target'/'snapshot-service-tests'
SCRATCH.mkdir(parents=True,exist_ok=True)


class SnapshotServiceTest(unittest.TestCase):
    def test_archive_streams_directly_to_receive_and_preserves_input(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);archive=root/'input.tar';received=[]
            boundaries=b'{"sequence":1,"snapshot":"snapshot-00000001"}\n{"sequence":2,"snapshot":"snapshot-00000002"}\n'
            with tarfile.open(archive,'w') as output:
                for name,data in [('boundaries.jsonl',boundaries),('markers.jsonl',b'fixture\n'),
                                  ('snapshot-00000001.btrfs',b'first'),('snapshot-00000002.btrfs',b'second')]:
                    info=tarfile.TarInfo(name);info.size=len(data);output.addfile(info,io.BytesIO(data))
            class Sink(io.BytesIO):
                def close(self):received.append(self.getvalue());super().close()
            def spawn(*_args,**_kwargs):
                return SimpleNamespace(stdin=Sink(),wait=lambda **_:0)
            with patch.object(asb_snapshot_guest.subprocess,'Popen',side_effect=spawn), \
                 patch.object(asb_snapshot_guest,'load_replay_boundaries',return_value=[('fixture',2)]):
                result=asb_snapshot_guest.receive_archive(archive,root/'received',['btrfs'])
            self.assertEqual(result,[('fixture',2)]);self.assertEqual(received,[b'first',b'second'])
            self.assertEqual((root/'received'/'boundaries.jsonl').read_bytes(),boundaries)
            self.assertTrue(archive.is_file())

    def test_archive_rejects_unlisted_snapshot_before_receive(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);archive=root/'input.tar'
            boundaries=b'{"sequence":1,"snapshot":"snapshot-00000001"}\n{"sequence":2,"snapshot":"snapshot-00000002"}\n'
            with tarfile.open(archive,'w') as output:
                for name,data in [('boundaries.jsonl',boundaries),('markers.jsonl',b''),
                                  ('snapshot-00000001.btrfs',b'first'),('snapshot-00000003.btrfs',b'wrong')]:
                    info=tarfile.TarInfo(name);info.size=len(data);output.addfile(info,io.BytesIO(data))
            with patch.object(asb_snapshot_guest.subprocess,'Popen') as spawn:
                with self.assertRaisesRegex(ValueError,'does not match its journal'):
                    asb_snapshot_guest.receive_archive(archive,root/'received',['btrfs'])
            spawn.assert_not_called();self.assertFalse((root/'received').exists());self.assertTrue(archive.is_file())

    def test_archive_receive_failure_retains_input_and_partial_output(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);archive=root/'input.tar'
            boundaries=b'{"sequence":1,"snapshot":"snapshot-00000001"}\n{"sequence":2,"snapshot":"snapshot-00000002"}\n'
            with tarfile.open(archive,'w') as output:
                for name,data in [('boundaries.jsonl',boundaries),('markers.jsonl',b''),
                                  ('snapshot-00000001.btrfs',b'first'),('snapshot-00000002.btrfs',b'second')]:
                    info=tarfile.TarInfo(name);info.size=len(data);output.addfile(info,io.BytesIO(data))
            with patch.object(asb_snapshot_guest.subprocess,'Popen',return_value=SimpleNamespace(stdin=io.BytesIO(),wait=lambda **_:1)):
                with self.assertRaisesRegex(RuntimeError,'retained partial output'):
                    asb_snapshot_guest.receive_archive(archive,root/'received',['btrfs'])
            self.assertTrue(archive.is_file());self.assertTrue((root/'received').is_dir())

    def test_guest_binds_all_roots_and_unmounts_in_reverse_order(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory).resolve();mounted=root/'mounted';mounted.mkdir()
            (mounted/'0').mkdir();(mounted/'1').mkdir()
            target=root/'task';extra=root/'temporary';target.mkdir();extra.mkdir()
            socket_path=root/'marker.sock';calls=[];closed=[]
            class Workspace:
                def __init__(self,*_args):self.mountpoint=root
                def prepare(self, source, extras):
                    self.assertions=(source,extras);return mounted
                def close(self):closed.append(True)
            def run(args,**_kwargs):calls.append(args)
            def unmount(target):calls.append(['umount',target])
            service=SimpleNamespace(finish=lambda:None,invalidate=lambda:None)
            args=['--target',str(target),'--extra-root',str(extra),'--root',str(root/'storage'),
                  '--socket',str(socket_path),'--output',str(root/'out'),'--btrfs','btrfs',
                  '--mkfs','mkfs','--rsync','rsync','--apply-final']
            with patch.object(asb_snapshot_guest,'SnapshotWorkspace',Workspace), \
                 patch.object(asb_snapshot_guest,'SnapshotService',return_value=service), \
                 patch.object(asb_snapshot_guest,'export_snapshot_streams'), \
                 patch.object(asb_snapshot_guest,'unmount_when_idle',side_effect=unmount), \
                 patch.object(asb_snapshot_guest.subprocess,'run',side_effect=run), \
                 patch.object(asb_snapshot_guest.sys,'stdin',io.StringIO('STOP\n')), \
                 patch.object(asb_snapshot_guest.sys,'stdout',io.StringIO()):
                asb_snapshot_guest.main(args)
            self.assertEqual(calls[0],['mount','--bind',str(mounted/'0'),str(target)])
            self.assertEqual(calls[1],['mount','--bind',str(mounted/'1'),str(extra)])
            self.assertEqual([x for x in calls if x[0]=='umount'],
                             [['umount',str(extra)],['umount',str(target)]])
            self.assertEqual([x[-1] for x in calls if x[0]=='rsync'],[str(extra)+'/',str(target)+'/'])
            self.assertEqual(closed,[True]);self.assertFalse(socket_path.exists())

    def test_guest_rejects_recording_without_matching_root_manifest(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory).resolve();mounted=root/'mounted';mounted.mkdir()
            baseline=root/'storage/filesystem/snapshots/snapshot-00000001'
            baseline.mkdir(parents=True);(baseline/'0').mkdir();(baseline/'1').mkdir()
            (baseline/'.asb-snapshot-roots.json').write_text(json.dumps(['/wrong','/tmp']))
            closed=[]
            class Workspace:
                def __init__(self,*_args):self.mountpoint=root
                def prepare(self,*_args,**_kwargs):return mounted
                def close(self):closed.append(True)
            args=['--target',str(root/'task'),'--extra-root',str(root/'temporary'),
                  '--root',str(root/'storage'),'--socket',str(root/'marker'),
                  '--output',str(root/'out'),'--btrfs','btrfs','--mkfs','mkfs',
                  '--rsync','rsync','--replay-archive',str(root/'archive')]
            # The supervisor owns creation of its storage directory.
            (root/'storage').rename(root/'prepared')
            def receive(_source,destination,_command):
                import shutil
                shutil.copytree(root/'prepared/filesystem/snapshots',destination)
                return []
            with patch.object(asb_snapshot_guest,'SnapshotWorkspace',Workspace), \
                 patch.object(asb_snapshot_guest,'receive_archive',side_effect=receive), \
                 patch.object(asb_snapshot_guest.subprocess,'run') as run:
                with self.assertRaisesRegex(ValueError,'does not match the configured roots'):
                    asb_snapshot_guest.main(args)
            run.assert_not_called();self.assertEqual(closed,[True])

    def test_guest_unmounts_first_root_if_second_mount_fails(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory).resolve();mounted=root/'mounted';mounted.mkdir()
            target=root/'task';extra=root/'temporary';target.mkdir();extra.mkdir()
            closed=[];unmounted=[]
            class Workspace:
                def __init__(self,*_args):pass
                def prepare(self,*_args):return mounted
                def close(self):closed.append(True)
            calls=[]
            def mount(args,**_kwargs):
                calls.append(args)
                if len(calls)==2:raise subprocess.CalledProcessError(1,args)
            args=['--target',str(target),'--extra-root',str(extra),'--root',str(root/'storage'),
                  '--socket',str(root/'marker'),'--output',str(root/'out'),'--btrfs','btrfs',
                  '--mkfs','mkfs','--rsync','rsync']
            with patch.object(asb_snapshot_guest,'SnapshotWorkspace',Workspace), \
                 patch.object(asb_snapshot_guest,'unmount_when_idle',side_effect=unmounted.append), \
                 patch.object(asb_snapshot_guest.subprocess,'run',side_effect=mount):
                with self.assertRaises(subprocess.CalledProcessError):
                    asb_snapshot_guest.main(args)
            self.assertEqual(unmounted,[str(target)]);self.assertEqual(closed,[True])

    def test_unmount_waits_for_transient_busy_without_forcing(self):
        busy=subprocess.CalledProcessError(32, ['umount','/task'], stderr='umount: /task: target is busy.\n')
        with patch.object(asb_snapshot_guest.subprocess, 'run', side_effect=[busy, None]) as run, \
             patch.object(asb_snapshot_guest.time, 'sleep') as sleep:
            asb_snapshot_guest.unmount_when_idle('/task')
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args.args[0], ['umount','/task'])
        sleep.assert_called_once()

    def test_unmount_does_not_retry_other_errors_or_unbounded_busy(self):
        denied=subprocess.CalledProcessError(1, ['umount','/task'], stderr='permission denied\n')
        busy=subprocess.CalledProcessError(32, ['umount','/task'], stderr='target is busy\n')
        for error, times in ((denied, [0, 1]), (busy, [0, 26])):
            with self.subTest(stderr=error.stderr), \
                 patch.object(asb_snapshot_guest.subprocess, 'run', side_effect=error) as run, \
                 patch.object(asb_snapshot_guest.time, 'monotonic', side_effect=times), \
                 patch.object(asb_snapshot_guest.time, 'sleep') as sleep, \
                 patch.object(asb_snapshot_guest.sys, 'stderr', io.StringIO()):
                with self.assertRaises(subprocess.CalledProcessError):
                    asb_snapshot_guest.unmount_when_idle('/task')
                self.assertEqual(run.call_count, 1)
                sleep.assert_not_called()

    def test_snapshot_cleanup_keeps_the_original_execution_failure(self):
        def fail():
            raise RuntimeError('snapshot replay did not settle')
        owner=SimpleNamespace(close=fail)
        with self.assertRaisesRegex(RuntimeError, 'agent launch failed; snapshot cleanup also failed: snapshot replay did not settle'):
            settle_on_exit(owner, RuntimeError, RuntimeError('agent launch failed'), None)
        with self.assertRaisesRegex(RuntimeError, 'snapshot replay did not settle'):
            settle_on_exit(owner, None, None, None)
        self.assertFalse(settle_on_exit(SimpleNamespace(close=lambda:None), RuntimeError, RuntimeError('original'), None))

    def test_guest_supervisor_uses_automatic_capacity_by_default(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);container=SimpleNamespace(id='fixture-container')
            task=SimpleNamespace(artifacts=root,workspace=SimpleNamespace(container=container))
            process=SimpleNamespace(stdout=io.BytesIO(b'READY\n'))
            tools={'btrfs':'/btrfs','mkfs.btrfs':'/mkfs','rsync':'/rsync'}
            with patch('asb_snapshot_control.upload'), \
                 patch('asb_snapshot_control.selectors.DefaultSelector') as selectors, \
                 patch('asb_snapshot_control.subprocess.Popen',return_value=process) as popen:
                selectors.return_value.__enter__.return_value.select.return_value=[True]
                supervisor=GuestSnapshot(task,tools,(0,0),['docker'],time.monotonic()+5)
            self.assertNotIn('--size',popen.call_args.args[0])
            supervisor.log.close()

    def test_guest_supervisor_passes_optional_qcow_tools(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);container=SimpleNamespace(id='fixture-container')
            task=SimpleNamespace(artifacts=root,workspace=SimpleNamespace(container=container))
            process=SimpleNamespace(stdout=io.BytesIO(b'READY\n'))
            tools={'btrfs':'/btrfs','mkfs.btrfs':'/mkfs','rsync':'/rsync',
                   'qemu-img':'/qemu-img','qemu-storage-daemon':'/qemu-storage'}
            with patch('asb_snapshot_control.upload'), \
                 patch('asb_snapshot_control.selectors.DefaultSelector') as selectors, \
                 patch('asb_snapshot_control.subprocess.Popen',return_value=process) as popen:
                selectors.return_value.__enter__.return_value.select.return_value=[True]
                supervisor=GuestSnapshot(task,tools,(0,0),['docker'],time.monotonic()+5)
            args=popen.call_args.args[0]
            self.assertEqual(args[args.index('--qemu-img')+1],'/qemu-img')
            self.assertEqual(args[args.index('--qemu-storage')+1],'/qemu-storage')
            supervisor.log.close()

    def test_guest_supervisor_forwards_extra_roots(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);container=SimpleNamespace(id='fixture-container')
            task=SimpleNamespace(artifacts=root,workspace=SimpleNamespace(container=container))
            process=SimpleNamespace(stdout=io.BytesIO(b'READY\n'))
            tools={'btrfs':'/btrfs','mkfs.btrfs':'/mkfs','rsync':'/rsync'}
            with patch('asb_snapshot_control.upload'), \
                 patch('asb_snapshot_control.selectors.DefaultSelector') as selectors, \
                 patch('asb_snapshot_control.subprocess.Popen',return_value=process) as popen:
                selectors.return_value.__enter__.return_value.select.return_value=[True]
                supervisor=GuestSnapshot(task,tools,(0,0),['docker'],time.monotonic()+5,extra_roots=['/tmp','/var/tmp'])
            args=popen.call_args.args[0]
            self.assertEqual([args[i+1] for i,value in enumerate(args) if value=='--extra-root'],['/tmp','/var/tmp'])
            supervisor.log.close()

    def test_replay_waits_without_tool_deadline_and_wakes_on_predecessor(self):
        applied=[];waiting=threading.Event()
        service=SnapshotReplay([('first',2),('second',3)],applied.append)
        wait=service.changed.wait
        def observed_wait(timeout=None):
            self.assertIsNone(timeout)
            waiting.set()
            return wait(timeout)
        with patch.object(service.changed,'wait',side_effect=observed_wait):
            with ThreadPoolExecutor(max_workers=1) as pool:
                later=pool.submit(service.marker,'end','second')
                self.assertTrue(waiting.wait(2))
                self.assertEqual(service.marker('end','first'),2)
                self.assertEqual(later.result(timeout=2),3)
        service.finish()
        self.assertEqual(applied,[2,3])

    def test_replay_invalidation_wakes_waiting_marker(self):
        waiting=threading.Event()
        service=SnapshotReplay([('first',2),('second',3)],lambda _:None)
        wait=service.changed.wait
        def observed_wait(timeout=None):
            waiting.set()
            return wait(timeout)
        with patch.object(service.changed,'wait',side_effect=observed_wait):
            with ThreadPoolExecutor(max_workers=1) as pool:
                later=pool.submit(service.marker,'end','second')
                self.assertTrue(waiting.wait(2))
                service.invalidate()
                with self.assertRaisesRegex(RuntimeError,'not active'):
                    later.result(timeout=2)
        with self.assertRaisesRegex(RuntimeError,'unconsumed or failed'):
            service.finish()

    def test_finish_waits_for_active_boundary_application(self):
        applying=threading.Event();release=threading.Event();finished=threading.Event()
        def apply(_sequence):
            applying.set()
            if not release.wait(2):raise RuntimeError('fixture did not release')
        service=SnapshotReplay([('first',2)],apply)
        def finish():
            service.finish();finished.set()
        with ThreadPoolExecutor(max_workers=2) as pool:
            marker=pool.submit(service.marker,'end','first')
            self.assertTrue(applying.wait(2));closing=pool.submit(finish)
            self.assertFalse(finished.is_set());release.set()
            self.assertEqual(marker.result(timeout=2),2);closing.result(timeout=2)
        self.assertTrue(finished.is_set())

    def test_closing_replay_releases_waiting_socket_handler(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            waiting=threading.Event()
            service=SnapshotReplay([('first',2),('second',3)],lambda _:None)
            wait=service.changed.wait
            def observed_wait(timeout=None):
                waiting.set()
                return wait(timeout)
            path=Path(directory)/'marker.sock';server=MarkerServer(path,service)
            worker=threading.Thread(target=server.serve_forever,daemon=True);worker.start()
            client=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);client.settimeout(2)
            try:
                with patch.object(service.changed,'wait',side_effect=observed_wait):
                    client.connect(str(path));client.sendall(b'tool-end:second\n')
                    self.assertTrue(waiting.wait(2))
                    server.shutdown()
                    with self.assertRaisesRegex(RuntimeError,'unconsumed or failed'):
                        service.finish()
                    server.server_close();worker.join(timeout=2)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(client.recv(32),b'')
            finally:
                service.invalidate();client.close();server.server_close();worker.join(timeout=2)

    def test_idle_marker_connection_is_interrupted_at_shutdown(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            accepted=threading.Event()
            service=SnapshotReplay([('first',2)],lambda _:None)
            path=Path(directory)/'marker.sock';server=MarkerServer(path,service)
            get_request=server.get_request
            def observed_request():
                client,address=get_request();accepted.set();return client,address
            with patch.object(server,'get_request',side_effect=observed_request):
                worker=threading.Thread(target=server.serve_forever,daemon=True);worker.start()
                client=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);client.settimeout(2)
                try:
                    client.connect(str(path));self.assertTrue(accepted.wait(2))
                    server.shutdown();service.invalidate();server.interrupt_clients()
                    server.server_close();worker.join(timeout=2)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(client.recv(32),b'')
                finally:
                    client.close();server.interrupt_clients();server.server_close();worker.join(timeout=2)

    def test_guest_shutdown_releases_a_missing_replay_predecessor(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            path=root/'marker.sock';waiting=threading.Event();closed=[];clients=[]
            class Workspace:
                def __init__(self,*args):self.mountpoint=workspace
                def prepare(self,_source,*_args,**_kwargs):return workspace
                def restore_baseline(self,_baseline):return workspace
                def close(self):closed.append(True)
            def replay(boundaries,apply):
                service=SnapshotReplay(boundaries,apply);wait=service.changed.wait
                def observed_wait(timeout=None):waiting.set();return wait(timeout)
                service.changed.wait=observed_wait
                return service
            class Input:
                def readline(self):
                    client=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);client.settimeout(2)
                    client.connect(str(path));client.sendall(b'tool-end:second\n');clients.append(client)
                    if not waiting.wait(2):raise AssertionError('marker did not wait')
                    return 'STOP\n'
            args=['--target',str(workspace),'--root',str(root/'storage'),'--socket',str(path),
                  '--output',str(root/'out'),'--btrfs','btrfs','--mkfs','mkfs',
                  '--rsync','rsync','--size','1024','--replay-archive',str(root/'archive')]
            try:
                with patch.object(asb_snapshot_guest,'SnapshotWorkspace',Workspace), \
                     patch.object(asb_snapshot_guest,'receive_archive',return_value=[('first',2),('second',3)]), \
                     patch.object(asb_snapshot_guest,'SnapshotApplier',return_value=lambda _:None), \
                     patch.object(asb_snapshot_guest,'SnapshotReplay',side_effect=replay), \
                     patch.object(asb_snapshot_guest.subprocess,'run'), \
                     patch.object(asb_snapshot_guest.sys,'stdin',Input()):
                    with self.assertRaisesRegex(RuntimeError,'unconsumed or failed'):
                        asb_snapshot_guest.main(args)
                self.assertEqual(closed,[True]);self.assertFalse(path.exists())
                self.assertEqual(clients[0].recv(32),b'')
            finally:
                for client in clients:client.close()

    def test_guest_shutdown_completes_consumed_replay(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            path=root/'marker.sock';closed=[];sizes=[]
            class Workspace:
                def __init__(self,*args):self.mountpoint=workspace;sizes.append(args[2])
                def prepare(self,_source,*_args,**_kwargs):return workspace
                def restore_baseline(self,_baseline):return workspace
                def close(self):closed.append(True)
            args=['--target',str(workspace),'--root',str(root/'storage'),'--socket',str(path),
                  '--output',str(root/'out'),'--btrfs','btrfs','--mkfs','mkfs',
                  '--rsync','rsync','--replay-archive',str(root/'archive')]
            output=io.StringIO()
            with patch.object(asb_snapshot_guest,'SnapshotWorkspace',Workspace), \
                 patch.object(asb_snapshot_guest,'receive_archive',return_value=[]), \
                 patch.object(asb_snapshot_guest,'SnapshotApplier',return_value=lambda _:None), \
                 patch.object(asb_snapshot_guest.subprocess,'run'), \
                 patch.object(asb_snapshot_guest.sys,'stdin',io.StringIO('STOP\n')), \
                 patch.object(asb_snapshot_guest.sys,'stdout',output):
                asb_snapshot_guest.main(args)
            self.assertEqual(output.getvalue(),'READY\nSAVED\n')
            self.assertEqual(sizes,[None])
            self.assertEqual(closed,[True]);self.assertFalse(path.exists())

    def test_standalone_service_shutdown_interrupts_idle_client(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            path=root/'marker.sock';accepted=threading.Event();clients=[]
            class ObservedServer(MarkerServer):
                def get_request(self):
                    client,address=super().get_request();accepted.set();return client,address
            def create_service(workspace,store,command):
                return SnapshotService(workspace,store,command,snapshot=lambda path:path.mkdir())
            class Input:
                def readline(self):
                    client=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);client.settimeout(2)
                    client.connect(str(path));clients.append(client)
                    if not accepted.wait(2):raise AssertionError('client was not accepted')
                    return 'STOP\n'
            try:
                with patch.object(asb_snapshot_service,'SnapshotService',side_effect=create_service), \
                     patch.object(asb_snapshot_service,'MarkerServer',ObservedServer), \
                     patch.object(asb_snapshot_service.sys,'stdin',Input()), \
                     patch.object(asb_snapshot_service.sys,'stdout',io.StringIO()):
                    asb_snapshot_service.main(['--workspace',str(workspace),'--store',str(root/'store'),
                                               '--socket',str(path),'--','btrfs'])
                self.assertFalse(path.exists());self.assertEqual(clients[0].recv(32),b'')
            finally:
                for client in clients:client.close()

    def test_snapshot_and_replay_sync_have_no_per_tool_subprocess_deadline(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            store=root/'store';store.mkdir()
            (store/'snapshot-00000001').mkdir();(store/'snapshot-00000002').mkdir()
            with patch('asb_snapshot_service.subprocess.run') as run:
                service=SnapshotService(workspace,root/'record',['btrfs'])
                service.marker('begin','tool');service.marker('end','tool');service.finish()
                SnapshotApplier(store,workspace,['rsync'])(2)
            self.assertGreaterEqual(run.call_count,3)
            self.assertTrue(all('timeout' not in call.kwargs for call in run.call_args_list))

    def test_snapshot_replay_accepts_only_matching_preapplied_file_additions(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);store=root/'store';store.mkdir();workspace=root/'workspace';workspace.mkdir()
            before=store/'snapshot-00000001';after=store/'snapshot-00000002';before.mkdir();after.mkdir()
            (after/'created').write_text('recorded');(workspace/'created').write_text('recorded')
            with patch('asb_snapshot_service.subprocess.run') as run:
                SnapshotApplier(store,workspace,['rsync'])(2)
            run.assert_called_once()
            (workspace/'created').write_text('different')
            with self.assertRaisesRegex(ValueError,'differs from its recorded snapshot'):
                SnapshotApplier(store,workspace,['rsync'])(2)
            (workspace/'unrelated').write_text('extra')
            with self.assertRaisesRegex(ValueError,'unexpected files'):
                SnapshotApplier(store,workspace,['rsync'])(2)

    def test_snapshot_replay_rejects_preapplied_file_with_wrong_hardlinks(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);store=root/'store';store.mkdir();workspace=root/'workspace';workspace.mkdir()
            before=store/'snapshot-00000001';after=store/'snapshot-00000002';before.mkdir();after.mkdir()
            (before/'existing').write_text('recorded');(after/'existing').write_text('recorded')
            os.link(after/'existing',after/'created')
            (workspace/'existing').write_text('recorded');(workspace/'created').write_text('recorded')
            with self.assertRaisesRegex(ValueError,'different hard-link topology'):
                SnapshotApplier(store,workspace,['rsync'])(2)

    def test_snapshot_settlement_points_to_retained_guest_failure(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);(root/'guest-error-stderr.log').write_text('failure')
            process=SimpleNamespace(returncode=1,communicate=lambda *_args,**_kwargs:(b'',None),poll=lambda:1)
            supervisor=GuestSnapshot.__new__(GuestSnapshot)
            supervisor.task=SimpleNamespace(artifacts=root);supervisor.process=process
            supervisor.log=io.BytesIO();supervisor.replay=False
            with self.assertRaisesRegex(RuntimeError,'guest-error-stderr.log'):
                supervisor.close()

    def test_snapshot_settlement_points_to_stdout_only_failure(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);(root/'guest-error-stdout.log').write_text('failure')
            process=unittest.mock.MagicMock();process.communicate.return_value=(b'',b'');process.returncode=1
            supervisor=GuestSnapshot.__new__(GuestSnapshot)
            supervisor.task=SimpleNamespace(artifacts=root);supervisor.process=process
            supervisor.log=io.BytesIO();supervisor.replay=False
            with self.assertRaisesRegex(RuntimeError,'guest-error-stdout.log'):
                supervisor.close()

    def test_snapshot_archive_streams_flat_entries_and_rejects_invalid_entries(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory)
            def archive(name, entries):
                path = root/name
                with tarfile.open(path, 'w') as output:
                    for entry, content, kind in entries:
                        info = tarfile.TarInfo(entry)
                        info.type = kind
                        info.size = len(content) if kind == tarfile.REGTYPE else 0
                        output.addfile(info, io.BytesIO(content) if info.size else None)
                return path
            valid = [('boundaries.jsonl', b'boundary', tarfile.REGTYPE),
                     ('markers.jsonl', b'marker', tarfile.REGTYPE),
                     ('snapshot-00000001.btrfs', b'stream', tarfile.REGTYPE)]
            unpack_streams(archive('valid.tar', valid), root/'valid')
            self.assertEqual((root/'valid/snapshot-00000001.btrfs').read_bytes(), b'stream')
            for index, entries in enumerate((
                    valid + [('../outside', b'x', tarfile.REGTYPE)],
                    valid + [('snapshot-00000001.btrfs', b'x', tarfile.REGTYPE)],
                    valid + [('snapshot-00000002.btrfs', b'', tarfile.SYMTYPE)],
                    valid[1:])):
                with self.subTest(index=index), self.assertRaises(ValueError):
                    unpack_streams(archive(f'invalid-{index}.tar', entries), root/f'invalid-{index}')
            self.assertFalse((root/'outside').exists())

    def test_overlapping_tools_produce_ordered_snapshots_and_final(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory); workspace=root/'workspace';workspace.mkdir()
            def capture(path):path.mkdir();(path/'value').write_bytes((workspace/'value').read_bytes())
            (workspace/'value').write_bytes(b'base')
            service=SnapshotService(workspace,root/'store',['fixture'],snapshot=capture)
            self.assertEqual(service.marker('begin','parent'),1)
            self.assertEqual(service.marker('begin','child'),1)
            (workspace/'value').write_bytes(b'child')
            self.assertEqual(service.marker('end','child'),2)
            (workspace/'value').write_bytes(b'parent')
            self.assertEqual(service.marker('end','parent'),3)
            self.assertEqual(service.finish(),4)
            entries=[json.loads(line) for line in (root/'store/boundaries.jsonl').read_text().splitlines()]
            self.assertEqual([(item['kind'],item['alias']) for item in entries],
                             [('baseline',None),('tool-end','child'),('tool-end','parent'),('final',None)])
            self.assertEqual([(root/'store'/item['snapshot']/'value').read_bytes() for item in entries],
                             [b'base',b'child',b'parent',b'parent'])
            events=[json.loads(line) for line in (root/'store/markers.jsonl').read_text().splitlines()]
            self.assertEqual([(e['sequence'],e['kind'],e['alias'],e['snapshot_sequence']) for e in events],
                             [(1,'begin','parent',1),(2,'begin','child',1),
                              (3,'end','child',2),(4,'end','parent',3)])

    def test_incomplete_and_failed_capture_do_not_publish_final(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            def capture(path):
                if path.name=='snapshot-00000002':raise RuntimeError('injected')
                path.mkdir()
            service=SnapshotService(workspace,root/'store',['fixture'],snapshot=capture)
            service.marker('begin','tool')
            with self.assertRaisesRegex(RuntimeError,'injected'):service.marker('end','tool')
            with self.assertRaisesRegex(RuntimeError,'unfinished'):service.finish()
            self.assertEqual(len((root/'store/boundaries.jsonl').read_text().splitlines()),1)

    def test_marker_journal_failure_invalidates_recording(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            service=SnapshotService(workspace,root/'store',['fixture'],snapshot=lambda path:path.mkdir())
            service.events.close()
            with self.assertRaises(ValueError):service.marker('begin','tool')
            with self.assertRaisesRegex(RuntimeError,'not active'):service.marker('begin','other')
            with self.assertRaisesRegex(RuntimeError,'unfinished'):service.finish()
            self.assertEqual(len((root/'store/boundaries.jsonl').read_text().splitlines()),1)

    def test_marker_server_accepts_adapter_protocol_and_rejects_bad_request(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            service=SnapshotService(workspace,root/'store',['fixture'],snapshot=lambda path:path.mkdir())
            path=root/'marker.sock';server=MarkerServer(path,service)
            thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
            def send(data):
                with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
                    client.settimeout(3);client.connect(str(path));client.sendall(data);return client.makefile('rb').readline()
            try:
                self.assertEqual(send(b'tool-begin:terminal-1:123:124\n'),b'1\n')
                self.assertEqual(send(b'tool-end:terminal-1:123:124\n'),b'2\n')
                with ThreadPoolExecutor(max_workers=4) as pool:
                    self.assertEqual(list(pool.map(lambda i:send(f'tool-begin:t{i}\n'.encode()),range(4))),[b'2\n']*4)
                    self.assertEqual(sorted(pool.map(lambda i:int(send(f'tool-end:t{i}\n'.encode())),range(4))),[3,4,5,6])
            finally:
                server.shutdown();server.server_close();thread.join();path.unlink(missing_ok=True)
            self.assertEqual(service.finish(),7)
            events=[json.loads(line) for line in (root/'store/markers.jsonl').read_text().splitlines()]
            self.assertEqual((events[0]['process'],events[0]['thread']),(123,124))
            self.assertEqual((events[1]['process'],events[1]['thread']),(123,124))
            self.assertEqual([event['sequence'] for event in events],list(range(1,11)))

    def test_guest_control_process_snapshots_and_settles(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir();(workspace/'value').write_bytes(b'base')
            fake=root/'fake_btrfs.py'
            fake.write_text('import shutil,sys\nshutil.copytree(sys.argv[-2],sys.argv[-1])\n')
            service=ROOT/'crates/asb-cli/assets/benchmarks/asb_snapshot_service.py'
            path=root/'socket'
            process=subprocess.Popen([sys.executable,str(service),'--workspace',str(workspace),'--store',str(root/'store'),
                                      '--socket',str(path),'--',sys.executable,str(fake)],
                                     stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
            try:
                self.assertEqual(process.stdout.readline(),'READY\n')
                def send(data):
                    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
                        client.settimeout(3);client.connect(str(path));client.sendall(data);return client.makefile('rb').readline()
                self.assertEqual(send(b'tool-begin:terminal-1:123:124\n'),b'1\n')
                (workspace/'value').write_bytes(b'changed')
                self.assertEqual(send(b'tool-end:terminal-1:123:124\n'),b'2\n')
                process.stdin.write('STOP\n');process.stdin.flush()
                _,stderr=process.communicate(timeout=10)
                self.assertEqual(process.returncode,0,stderr)
                self.assertFalse(path.exists())
                self.assertEqual((root/'store/snapshot-00000001/value').read_bytes(),b'base')
                self.assertEqual((root/'store/snapshot-00000002/value').read_bytes(),b'changed')
                self.assertEqual((root/'store/snapshot-00000003/value').read_bytes(),b'changed')
            finally:
                if process.poll() is None:
                    process.kill();process.communicate()

    def test_malformed_socket_marker_prevents_completion(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            service=SnapshotService(workspace,root/'store',['fixture'],snapshot=lambda path:path.mkdir())
            path=root/'marker.sock';server=MarkerServer(path,service)
            thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
            try:
                with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
                    client.settimeout(3);client.connect(str(path));client.sendall(b'invalid\n')
                    self.assertEqual(client.makefile('rb').readline(),b'')
            finally:
                server.shutdown();server.server_close();thread.join();path.unlink(missing_ok=True)
            with self.assertRaisesRegex(RuntimeError,'unfinished'):service.finish()

    def test_duplicate_socket_marker_invalidates_incomplete_recording(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=root/'workspace';workspace.mkdir()
            service=SnapshotService(workspace,root/'store',['fixture'],snapshot=lambda path:path.mkdir())
            path=root/'marker.sock';server=MarkerServer(path,service)
            thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
            def send(data):
                with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
                    client.settimeout(3);client.connect(str(path));client.sendall(data)
                    return client.makefile('rb').readline()
            try:
                self.assertEqual(send(b'tool-begin:one:10:11\n'),b'1\n')
                self.assertEqual(send(b'tool-begin:one:10:11\n'),b'')
                self.assertEqual(send(b'tool-end:one:10:11\n'),b'')
            finally:
                server.shutdown();server.server_close();thread.join();path.unlink(missing_ok=True)
            with self.assertRaisesRegex(RuntimeError,'unfinished'):service.finish()
            events=[json.loads(line) for line in (root/'store/markers.jsonl').read_text().splitlines()]
            self.assertEqual([(event['kind'],event['alias']) for event in events],[('begin','one')])


if __name__=='__main__':unittest.main()
