"""Compact tool reads retain validation while direct reads retain their bytes."""
import hashlib
import io
import json
from pathlib import Path
import sys
import tempfile
import threading
import types
import unittest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_fs_attribution import Attribution
from asb_fs_replay_cursor import Replay

source = (ROOT/'crates/asb-cli/assets/benchmarks/asb_fs_proxy.py').read_text().rsplit('\nFUSE(Proxy()', 1)[0]
fuse = types.ModuleType('fuse'); fuse.FUSE = lambda *_args, **_kwargs: None
fuse.FuseOSError = OSError; fuse.Operations = type('Operations', (), {})
fuse.fuse_get_context = lambda: (0, 0, 0); sys.modules['fuse'] = fuse
argv=sys.argv;sys.argv=['fixture','/tmp/backing','/tmp/mount','/tmp/evidence']
module={'__name__':'fs_compact_fixture'}
try: exec(compile(source,'asb_fs_proxy.py','exec'), module)
finally: sys.argv=argv

class CompactReadTest(unittest.TestCase):
    def test_only_uniquely_attributed_tool_reads_are_compacted(self):
        proxy=module['Proxy'].__new__(module['Proxy']); proxy.record_lock=threading.Lock()
        proxy.sequence=0; proxy.log=io.StringIO();proxy.attribution=Attribution()
        class Payload:
            def __init__(self): self.data=bytearray()
            def append(self, data): offset=len(self.data);self.data.extend(data);return offset
        proxy.payload=Payload()
        parent={'tgid':100,'start_ticks':11}
        current=[{'tid':101,'tgid':100,'start_ticks':12,'ancestry':[parent],
                  'ancestry_complete':True,'thread_ancestry_complete':True}]
        module['caller_identity']=lambda _tid:current[0]
        proxy.record('open','/direct',handle=1,flags=0)
        proxy.record('read','/direct',b'abc',handle=1,offset=0,requested_size=3)
        proxy.record('release','/direct',handle=1)
        proxy.record('marker',None,marker='tool-begin:a',origin={'pid':100,'tid':102,'process_start_ticks':11,'thread_start_ticks':13},is_marker=True)
        current[0]={'tid':102,'tgid':100,'start_ticks':13,'ancestry':[parent],
                    'ancestry_complete':True,'thread_ancestry_complete':True}
        proxy.record('open','/tool',handle=2,flags=0)
        proxy.record('read','/tool',b'abc',handle=2,offset=0,requested_size=3)
        proxy.record('release','/tool',handle=2)
        proxy.record('marker',None,marker='tool-end:a',origin=None,is_marker=True)
        rows=[json.loads(line) for line in proxy.log.getvalue().splitlines()]
        self.assertEqual(rows[1]['object'],'payload.bin')
        self.assertEqual(rows[5]['payload_sha256'],hashlib.sha256(b'abc').hexdigest())
        self.assertNotIn('object',rows[5]);self.assertEqual(proxy.payload.data,b'abc')
    def test_attribution_failure_falls_back_to_full_payload(self):
        proxy=module['Proxy'].__new__(module['Proxy']); proxy.record_lock=threading.Lock()
        proxy.sequence=0; proxy.log=io.StringIO()
        class Broken:
            def observe(self, _event): raise ValueError('fixture')
        class Payload:
            def __init__(self): self.data=bytearray()
            def append(self, data): self.data.extend(data);return 0
        proxy.attribution=Broken();proxy.payload=Payload()
        module['caller_identity']=lambda _tid: None
        proxy.record('read','/file',b'abc',handle=1,offset=0,requested_size=3)
        row=json.loads(proxy.log.getvalue())
        self.assertIsNone(proxy.attribution)
        self.assertEqual(row['object'],'payload.bin');self.assertEqual(proxy.payload.data,b'abc')
        self.assertNotIn('payload_sha256',row)

    def test_digest_replay_validates_content_and_rejects_tampering(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            root=Path(directory); evidence=root/'evidence';evidence.mkdir(); workspace=root/'workspace';workspace.mkdir()
            target=workspace/'file';target.write_bytes(b'abcdef'); (evidence/'payload.bin').write_bytes(b'')
            events=[{'sequence':1,'op':'open','path':'/file','handle':1,'flags':0},
                    {'sequence':2,'op':'read','path':'/file','handle':1,'offset':1,'bytes':3,'requested_size':3,'payload_sha256':hashlib.sha256(b'bcd').hexdigest()},
                    {'sequence':3,'op':'release','path':'/file','handle':1}]
            (evidence/'events.jsonl').write_text(''.join(json.dumps(e)+'\n' for e in events))
            replay=Replay(evidence,workspace,existing=True);replay.finish();replay.close()
            target.write_bytes(b'axcdef')
            replay=Replay(evidence,workspace,existing=True)
            with self.assertRaisesRegex(RuntimeError,'recorded read differs'): replay.finish()
            replay.close()
            events[1]['payload_sha256']='bad';(evidence/'events.jsonl').write_text(''.join(json.dumps(e)+'\n' for e in events))
            replay=Replay(evidence,workspace,existing=True)
            with self.assertRaisesRegex(RuntimeError,'digest is invalid'): replay.finish()
            replay.close()

if __name__=='__main__':unittest.main()
