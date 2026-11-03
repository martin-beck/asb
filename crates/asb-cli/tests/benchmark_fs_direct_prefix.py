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
            with self.assertRaisesRegex(ValueError, r'read group differs at event 2'):
                validate_direct_prefix(encode(expected),payload,encode(changed),payload)
            changed = events(20); changed[0]['path'] = '/private-sensitive-path'
            with self.assertRaises(ValueError) as caught:
                validate_direct_prefix(encode(expected),payload,encode(changed),payload)
            self.assertNotIn('/private-sensitive-path', str(caught.exception))
            self.assertIn('event 1 (path)', str(caught.exception))
            with self.assertRaisesRegex(ValueError,'read group differs'):
                validate_direct_prefix(encode(expected),payload,encode(events(20)),other)
            missing = events(20); missing[0]['caller'] = None
            with self.assertRaisesRegex(ValueError,'unproven attribution'):
                validate_direct_prefix(encode(expected),payload,encode(missing),payload)
            extra = events(20)+[{'sequence':8,'op':'unlink','path':'/other','caller':CALLER}]
            with self.assertRaisesRegex(ValueError,'additional operations'):
                validate_direct_prefix(encode(expected),payload,encode(extra),payload)
    def test_concurrent_reads_reorder_only_within_the_same_handle_group(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            payload = Path(directory)/'payload.bin'; payload.write_bytes(b'oldnew')
            expected = events()
            first = dict(expected[1], offset=0, bytes=1, requested_size=1)
            second = dict(expected[1], offset=1, bytes=2, requested_size=2, payload_offset=1)
            expected[1:2] = [first, second]
            for index, item in enumerate(expected, 1): item['sequence'] = index
            expected.append({'sequence':9,'op':'marker','marker':'tool-begin:first','origin':None})
            actual = [dict(item) for item in expected[:-1]]
            actual[1], actual[2] = actual[2], actual[1]
            for index, item in enumerate(actual, 1): item['sequence'] = index
            self.assertEqual(validate_direct_prefix(encode(expected),payload,encode(actual),payload), 8)
            actual[1]['requested_size'] = 3
            with self.assertRaisesRegex(ValueError, 'read group differs'):
                validate_direct_prefix(encode(expected),payload,encode(actual),payload)
            actual[1]['requested_size'] = 2
            actual[1], actual[3] = actual[3], actual[1]
            for index, item in enumerate(actual, 1): item['sequence'] = index
            with self.assertRaises(ValueError):
                validate_direct_prefix(encode(expected),payload,encode(actual),payload)

    def test_reads_on_independent_handles_can_finish_in_any_order(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            payload = Path(directory)/'payload.bin'; payload.write_bytes(b'oldnew')
            original = events()
            other_open = dict(original[0], handle=3, path='/other')
            other_read = dict(original[1], handle=3, path='/other', payload_offset=3)
            other_release = {'op':'release','path':'/other','handle':3,'caller':None}
            expected = [original[0], other_open, original[1], other_read, original[2], other_release] + original[3:]
            for index, item in enumerate(expected, 1): item['sequence'] = index
            expected.append({'sequence':len(expected)+1,'op':'marker','marker':'tool-begin:first','origin':None})
            actual = [dict(item) for item in expected[:-1]]
            actual[2], actual[3] = actual[3], actual[2]
            for index, item in enumerate(actual, 1): item['sequence'] = index
            self.assertEqual(validate_direct_prefix(encode(expected), payload, encode(actual), payload), 10)
            actual[2]['handle'] = 1
            with self.assertRaisesRegex(ValueError, 'read group differs'):
                validate_direct_prefix(encode(expected), payload, encode(actual), payload)

    def test_rejects_unclosed_handles_and_old_missing_request_metadata(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            payload = Path(directory)/'payload.bin'; payload.write_bytes(b'oldnew')
            partial = events()[:2]+[{'sequence':3,'op':'marker','marker':'tool-begin:first','origin':None}]
            with self.assertRaisesRegex(ValueError,'open handles'):
                validate_direct_prefix(encode(partial),payload,encode(events(20)[:2]),payload)
            old = events(); del old[1]['requested_size']
            old += [{'sequence':8,'op':'marker','marker':'tool-begin:first','origin':None}]
            with self.assertRaisesRegex(ValueError,'incomplete'):
                validate_direct_prefix(encode(old),payload,encode(events(20)),payload)

if __name__ == '__main__': unittest.main()
