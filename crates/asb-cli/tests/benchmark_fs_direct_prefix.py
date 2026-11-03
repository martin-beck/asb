"""Direct startup effects must match before recorded operations can be skipped."""
import json
import hashlib
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_fs_direct_prefix import validate_direct_prefix

CALLER = {'tid':101,'tgid':100,'start_ticks':12,'ancestry_complete':True,
          'thread_ancestry_complete':True,'ancestry':[{'tgid':100,'start_ticks':11}]}

def events(handle_delta=0):
    first, second = 1+handle_delta, 2+handle_delta
    return [
        {'sequence':1,'op':'open','path':'/source','flags':0,'initial_size':3,'initial_mode':33188,'initial_nlink':1,'handle':first,'caller':CALLER},
        {'sequence':2,'op':'read','path':'/source','offset':0,'requested_size':16,'bytes':3,'object':'payload.bin','payload_offset':0,'handle':first,'caller':CALLER},
        {'sequence':3,'op':'release','path':'/source','handle':first,'caller':None},
        {'sequence':4,'op':'create','path':'/temp','flags':1,'mode':420,'handle':second,'caller':CALLER},
        {'sequence':5,'op':'write','path':'/temp','offset':0,'requested_size':3,'request_sha256':hashlib.sha256(b'new').hexdigest(),'bytes':3,'object':'payload.bin','payload_offset':3,'handle':second,'caller':CALLER},
        {'sequence':6,'op':'release','path':'/temp','handle':second,'caller':None},
        {'sequence':7,'op':'rename','path':'/temp','destination':'/output','caller':CALLER},
    ]

def encode(items): return [json.dumps(item)+'\n' for item in items]

class DirectPrefixTest(unittest.TestCase):
    def test_matches_effects_with_distinct_runtime_handles(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            payload = Path(directory)/'payload.bin'; payload.write_bytes(b'oldnew')
            expected = events()+[{'sequence':8,'op':'marker','marker':'tool-begin:first','origin':None}]
            self.assertEqual(validate_direct_prefix(encode(expected), payload, encode(events(20)), payload), 7)
    def test_rejects_changed_request_payload_missing_lineage_and_extra_operations(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            payload = Path(directory)/'payload.bin'; payload.write_bytes(b'oldnew')
            other = Path(directory)/'different.bin'; other.write_bytes(b'OLDnew')
            expected = events()+[{'sequence':8,'op':'marker','marker':'tool-begin:first','origin':None}]
            changed = events(20); changed[1]['requested_size'] = 15
            with self.assertRaisesRegex(ValueError,'differs'):
                validate_direct_prefix(encode(expected),payload,encode(changed),payload)
            with self.assertRaisesRegex(ValueError,'payload differs'):
                validate_direct_prefix(encode(expected),payload,encode(events(20)),other)
            missing = events(20); missing[0]['caller'] = None
            with self.assertRaisesRegex(ValueError,'unproven attribution'):
                validate_direct_prefix(encode(expected),payload,encode(missing),payload)
            extra = events(20)+[{'sequence':8,'op':'unlink','path':'/other','caller':CALLER}]
            with self.assertRaisesRegex(ValueError,'additional operations'):
                validate_direct_prefix(encode(expected),payload,encode(extra),payload)
    def test_rejects_unclosed_handles_and_old_missing_request_metadata(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            payload = Path(directory)/'payload.bin'; payload.write_bytes(b'oldnew')
            partial = events()[:2]+[{'sequence':3,'op':'marker','marker':'tool-begin:first','origin':None}]
            with self.assertRaisesRegex(ValueError,'open handles'):
                validate_direct_prefix(encode(partial),payload,encode(events(20)[:2]),payload)
            old = events(); del old[1]['requested_size']
            old += [{'sequence':8,'op':'marker','marker':'tool-begin:first','origin':None}]
            with self.assertRaisesRegex(ValueError,'differs'):
                validate_direct_prefix(encode(old),payload,encode(events(20)),payload)

if __name__ == '__main__': unittest.main()
