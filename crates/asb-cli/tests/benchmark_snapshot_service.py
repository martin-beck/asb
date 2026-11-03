"""Offline tests for snapshot lifecycle and adapter marker protocol."""
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import unittest

ROOT=Path(__file__).resolve().parents[3]
sys.path.insert(0,str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_snapshot_service import MarkerServer,SnapshotService

SCRATCH=ROOT/'target'/'snapshot-service-tests'
SCRATCH.mkdir(parents=True,exist_ok=True)


class SnapshotServiceTest(unittest.TestCase):
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
