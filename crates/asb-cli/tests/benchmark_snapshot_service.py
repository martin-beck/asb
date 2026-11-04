"""Offline tests for snapshot lifecycle and adapter marker protocol."""
from concurrent.futures import ThreadPoolExecutor
import json
import io
from pathlib import Path
import socket
import shutil
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
    @unittest.expectedFailure
    def test_replay_preserves_direct_write_after_last_tool(self):
        # Direct agent/subagent writes have no tool marker.  A successful
        # boundary replay must not be mistaken for full workspace replay.
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);record=root/'record';record.mkdir()
            store=root/'store';replay=root/'replay';replay.mkdir()
            service=SnapshotService(record,store,['fixture'],snapshot=lambda path:shutil.copytree(record,path))
            service.marker('begin','tool');(record/'tool.txt').write_text('tool effect')
            service.marker('end','tool')
            (record/'direct.txt').write_text('direct agent effect')
            service.finish()
            boundaries=asb_snapshot_service.load_replay_boundaries(store)
            def apply(sequence):
                for item in replay.iterdir():item.unlink()
                for item in (store/f'snapshot-{sequence:08d}').iterdir():
                    shutil.copy2(item,replay/item.name)
            restored=SnapshotReplay(boundaries,apply)
            restored.marker('end','tool');restored.finish()
            self.assertEqual((replay/'direct.txt').read_text(), 'direct agent effect')

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
                def prepare(self,_source):return workspace
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
                     patch.object(asb_snapshot_guest,'unpack_streams'), \
                     patch.object(asb_snapshot_guest,'receive_snapshot_streams',return_value=[('first',2),('second',3)]), \
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
                def prepare(self,_source):return workspace
                def close(self):closed.append(True)
            args=['--target',str(workspace),'--root',str(root/'storage'),'--socket',str(path),
                  '--output',str(root/'out'),'--btrfs','btrfs','--mkfs','mkfs',
                  '--rsync','rsync','--replay-archive',str(root/'archive')]
            output=io.StringIO()
            with patch.object(asb_snapshot_guest,'SnapshotWorkspace',Workspace), \
                 patch.object(asb_snapshot_guest,'unpack_streams'), \
                 patch.object(asb_snapshot_guest,'receive_snapshot_streams',return_value=[]), \
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
