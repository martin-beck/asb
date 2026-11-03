"""Replay metadata without crossing a recorded mutation boundary."""
import errno
from pathlib import Path
import sys
import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor

ROOT=Path(__file__).resolve().parents[3]
sys.path.insert(0,str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_fs_metadata_windows import ObservationWindow


def attrs(size):
    return dict(st_gid=1,st_ino=1,st_mode=33188,st_nlink=1,st_size=size,st_uid=1,
                st_blocks=1,st_blksize=4096,st_atime=1,st_mtime=1,st_ctime=1)


class MetadataWindowTest(unittest.TestCase):
    def test_independent_reads_reorder_but_mutations_are_barriers(self):
        events=[{'op':'observe-getattr','path':'/a','result':attrs(1)},
                {'op':'observe-getattr','path':'/b','result':attrs(2)},
                {'op':'write','path':'/a'},
                {'op':'observe-getattr','path':'/a','result':attrs(3)}]
        window=ObservationWindow(events,ROOT/'target/unused')
        self.assertEqual(window.observe('observe-getattr','/b'),attrs(2))
        with self.assertRaisesRegex(RuntimeError,'remain'): window.advance()
        self.assertEqual(window.observe('observe-getattr','/a'),attrs(1))
        self.assertEqual(window.advance(),{'op':'write','path':'/a'})
        with self.assertRaisesRegex(RuntimeError,'not been applied'): window.observe('observe-getattr','/a')
        with self.assertRaisesRegex(RuntimeError,'not been applied'): window.advance()
        window.operation_applied()
        self.assertEqual(window.observe('observe-getattr','/a'),attrs(3))
        self.assertIsNone(window.advance())
        with self.assertRaisesRegex(RuntimeError,'not ready'): window.operation_applied()

    def test_repeated_key_preserves_order_and_errors(self):
        events=[{'op':'observe-getattr','path':'/a','error':errno.ENOENT},
                {'op':'observe-getattr','path':'/a','result':attrs(1)}]
        window=ObservationWindow(events,ROOT/'target/unused')
        with self.assertRaises(OSError) as caught: window.observe('observe-getattr','/a')
        self.assertEqual(caught.exception.errno,errno.ENOENT)
        self.assertEqual(window.observe('observe-getattr','/a'),attrs(1))
        with self.assertRaisesRegex(RuntimeError,'no recorded'): window.observe('observe-getattr','/a')
        self.assertIsNone(window.advance())

    def test_extended_attribute_payload_and_truncation(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as tmp:
            payload=Path(tmp)/'payload.bin';payload.write_bytes(b'onevalue')
            event={'op':'observe-getxattr','path':'/a','name':'user.test','position':0,
                   'object':'payload.bin','payload_offset':3,'bytes':5}
            window=ObservationWindow([event],payload)
            self.assertEqual(window.observe('observe-getxattr','/a',name='user.test',position=0),b'value')
            self.assertIsNone(window.advance())
            event=dict(event,bytes=6);window=ObservationWindow([event],payload)
            with self.assertRaisesRegex(ValueError,'truncated'): window.observe('observe-getxattr','/a',name='user.test',position=0)

    def test_four_replicas_keep_independent_cursors(self):
        events=[{'op':'observe-getattr','path':'/a','result':attrs(1)},
                {'op':'write','path':'/a'},
                {'op':'observe-getattr','path':'/a','result':attrs(2)}]
        def replica(_):
            window=ObservationWindow(events,ROOT/'target/unused')
            first=window.observe('observe-getattr','/a')
            self.assertEqual(window.advance()['op'],'write')
            window.operation_applied()
            second=window.observe('observe-getattr','/a')
            self.assertIsNone(window.advance())
            return first,second
        with ThreadPoolExecutor(max_workers=4) as pool:
            self.assertEqual(list(pool.map(replica,range(4))),[(attrs(1),attrs(2))]*4)

    def test_concurrent_callers_consume_each_record_once(self):
        events=[{'op':'observe-getattr','path':'/a','result':attrs(i)} for i in range(32)]
        window=ObservationWindow(events,ROOT/'target/unused')
        with ThreadPoolExecutor(max_workers=8) as pool:
            results=list(pool.map(lambda _:window.observe('observe-getattr','/a')['st_size'],range(32)))
        self.assertEqual(sorted(results),list(range(32)))
        self.assertIsNone(window.advance())

    def test_wrong_request_does_not_consume_observation(self):
        window=ObservationWindow([{'op':'observe-readlink','path':'/a','result':'target'}],ROOT/'target/unused')
        with self.assertRaisesRegex(RuntimeError,'no recorded'): window.observe('observe-readlink','/b')
        self.assertEqual(window.observe('observe-readlink','/a'),'target')
        self.assertIsNone(window.advance())

if __name__=='__main__': unittest.main()
