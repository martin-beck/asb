"""Recorded metadata results are checked before tool replay progresses."""
import errno
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'crates/asb-cli/assets/benchmarks'))
from asb_fs_replay_cursor import Replay
from asb_fs_direct_prefix import validate_direct_prefix
from benchmark_fs_direct_prefix import CALLER


class MetadataReplayTest(unittest.TestCase):
    def test_matching_and_changed_observations(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            root = Path(directory); evidence = root/'evidence'; evidence.mkdir(); workspace=root/'workspace'; workspace.mkdir()
            (evidence/'events.jsonl').write_text(''); (evidence/'payload.bin').write_bytes(b'')
            (workspace/'file').write_bytes(b'hello'); (workspace/'link').symlink_to('file')
            replay = Replay(evidence, workspace, existing=True)
            st = os.lstat(workspace/'file')
            attrs = {key:getattr(st,key) for key in ('st_gid','st_ino','st_mode','st_nlink','st_size','st_uid','st_blocks','st_blksize')}
            attrs.update(st_atime=st.st_atime_ns, st_mtime=st.st_mtime_ns, st_ctime=st.st_ctime_ns)
            replay.apply({'op':'observe-getattr','path':'/file','result':attrs})
            replay.apply({'op':'observe-readlink','path':'/link','result':'file'})
            replay.apply({'op':'observe-readdir','path':'/','result':['.','..',*os.listdir(workspace)]})
            replay.apply({'op':'observe-getattr','path':'/missing','error':errno.ENOENT})
            attrs['st_size'] = 999
            with self.assertRaisesRegex(RuntimeError, 'observation differs'):
                replay.apply({'op':'observe-getattr','path':'/file','result':attrs})
            with self.assertRaisesRegex(RuntimeError, 'unexpectedly succeeded'):
                replay.apply({'op':'observe-getattr','path':'/file','error':errno.ENOENT})
            with self.assertRaisesRegex(RuntimeError, 'error differs'):
                replay.apply({'op':'observe-getattr','path':'/missing','error':errno.EACCES})
            replay.close()

    def test_extended_attribute_payload_is_checked(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            root=Path(directory); evidence=root/'evidence'; evidence.mkdir(); workspace=root/'workspace'; workspace.mkdir()
            (evidence/'events.jsonl').write_text(''); (evidence/'payload.bin').write_bytes(b'value')
            replay=Replay(evidence,workspace,existing=True)
            event={'op':'observe-getxattr','path':'/file','name':'user.example','position':0,
                   'object':'payload.bin','payload_offset':0,'bytes':5}
            with patch('os.getxattr',return_value=b'value'):
                replay.apply(event)
            with patch('os.getxattr',return_value=b'other'):
                with self.assertRaisesRegex(RuntimeError,'observation differs'): replay.apply(event)
            with patch('os.getxattr',return_value=b'value'):
                with self.assertRaisesRegex(RuntimeError,'invalid'):
                    replay.apply(dict(event,error=errno.ENOENT))
            replay.close()

    def test_direct_observation_results_are_compared(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            payload=Path(directory)/'payload.bin';payload.write_bytes(b'')
            event={'sequence':1,'op':'observe-getattr','path':'/file','result':{'st_size':3},'caller':CALLER}
            end={'sequence':2,'op':'marker','marker':'tool-begin:first','origin':None}
            lines=lambda entries: [json.dumps(item)+'\n' for item in entries]
            self.assertEqual(validate_direct_prefix(lines([event,end]),payload,lines([event]),payload),1)
            changed=dict(event,result={'st_size':4})
            with self.assertRaisesRegex(ValueError,'observation differs'):
                validate_direct_prefix(lines([event,end]),payload,lines([changed]),payload)
            changed=dict(event);changed.pop('result');changed['error']=errno.ENOENT
            with self.assertRaisesRegex(ValueError,'observation differs'):
                validate_direct_prefix(lines([event,end]),payload,lines([changed]),payload)


if __name__ == '__main__': unittest.main()
