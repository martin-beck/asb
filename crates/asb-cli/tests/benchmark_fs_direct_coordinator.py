"""Validate direct effects before advancing the tool replay coordinator."""
import hashlib
import json
from pathlib import Path
import socket
import sys
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[3]
sys.path.insert(0,str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_fs_replay_server import ReplayServer

TOOL={'tid':102,'tgid':100,'start_ticks':13,'ancestry_complete':True,
      'thread_ancestry_complete':True,'ancestry':[{'tgid':100,'start_ticks':11}]}
ORIGIN={'pid':100,'tid':102,'process_start_ticks':11,'thread_start_ticks':13}
CALLER={'tid':101,'tgid':100,'start_ticks':12,'ancestry_complete':True,
        'thread_ancestry_complete':True,'ancestry':[{'tgid':100,'start_ticks':11}]}

def setup(root):
    evidence=root/'evidence';evidence.mkdir();trace=root/'trace';trace.mkdir();workspace=root/'workspace';workspace.mkdir()
    (workspace/'file').write_bytes(b'new')
    digest=hashlib.sha256(b'new').hexdigest()
    prefix=[
      {'sequence':1,'op':'create','path':'/temp','flags':1,'mode':420,'handle':1,'caller':CALLER},
      {'sequence':2,'op':'write','path':'/temp','offset':0,'handle':1,'bytes':3,'requested_size':3,'request_sha256':digest,'object':'payload.bin','payload_offset':0,'caller':CALLER},
      {'sequence':3,'op':'release','path':'/temp','handle':1,'caller':None},
      {'sequence':4,'op':'rename','path':'/temp','destination':'/file','caller':CALLER},
    ]
    tool=[{'sequence':5,'op':'marker','path':None,'marker':'tool-begin:a','origin':ORIGIN},
          {'sequence':6,'op':'open','path':'/file','flags':0,'handle':2,'caller':TOOL},
          {'sequence':7,'op':'read','path':'/file','offset':0,'handle':2,'bytes':3,'object':'payload.bin','payload_offset':3,'caller':TOOL},
          {'sequence':8,'op':'release','path':'/file','handle':2},
          {'sequence':9,'op':'marker','path':None,'marker':'tool-end:a'}]
    (evidence/'events.jsonl').write_text(''.join(json.dumps(e)+'\n' for e in prefix+tool));(evidence/'payload.bin').write_bytes(b'newnew')
    (trace/'events.jsonl').write_text(''.join(json.dumps(e)+'\n' for e in prefix));(trace/'payload.bin').write_bytes(b'new')
    return evidence,trace,workspace

def invoke(path):
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
        client.settimeout(3);client.connect(str(path));client.sendall(b'tool-end:a\n')
        return client.makefile('rb').readline()

class DirectCoordinatorTest(unittest.TestCase):
    def test_valid_direct_prefix_is_not_applied_twice(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            root=Path(directory);evidence,trace,workspace=setup(root)
            server=ReplayServer(evidence,workspace,root/'socket',timeout=2,direct_trace=trace)
            self.assertEqual(invoke(root/'socket'),b'9\n')
            self.assertEqual((workspace/'file').read_bytes(),b'new');self.assertFalse((workspace/'temp').exists())
            server.finish()
    def test_changed_direct_trace_is_rejected_before_tool_effects(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            root=Path(directory);evidence,trace,workspace=setup(root)
            (trace/'payload.bin').write_bytes(b'bad')
            server=ReplayServer(evidence,workspace,root/'socket',timeout=2,direct_trace=trace)
            self.assertEqual(invoke(root/'socket'),b'')
            with self.assertRaisesRegex(RuntimeError,'did not complete'):server.finish()
            self.assertEqual((workspace/'file').read_bytes(),b'new')
    def test_recorded_direct_operations_after_startup_are_not_silently_applied(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            root=Path(directory);evidence,trace,workspace=setup(root)
            events=[json.loads(line) for line in (evidence/'events.jsonl').read_text().splitlines()]
            events.insert(5, {'op':'open','path':'/file','flags':0,'handle':7,'caller':CALLER})
            events.insert(6, {'op':'release','path':'/file','handle':7,'caller':None})
            for index,event in enumerate(events,1):event['sequence']=index
            (evidence/'events.jsonl').write_text(''.join(json.dumps(event)+'\n' for event in events))
            server=ReplayServer(evidence,workspace,root/'socket',timeout=2,direct_trace=trace)
            self.assertEqual(invoke(root/'socket'),b'')
            with self.assertRaisesRegex(RuntimeError,'did not complete'):server.finish()
            self.assertEqual((workspace/'file').read_bytes(),b'new')

    def test_direct_operations_after_validation_are_reported(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            root=Path(directory);evidence,trace,workspace=setup(root)
            server=ReplayServer(evidence,workspace,root/'socket',timeout=2,direct_trace=trace)
            self.assertEqual(invoke(root/'socket'),b'9\n')
            with (trace/'events.jsonl').open('a') as stream:stream.write('{}\n')
            with self.assertRaisesRegex(RuntimeError,'changed after'):server.finish()

if __name__=='__main__':unittest.main()
