"""Offline coverage for conservative initial-read planning."""
import io
import json
import os
from pathlib import Path
import stat
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1] / 'assets/benchmarks'))
from asb_fs_initial_reads import plan_initial_reads


def events(*items):
    return io.StringIO(''.join(json.dumps(dict(sequence=i, **event)) + '\n'
                               for i, event in enumerate(items, 1)))


def opened(path='/file', handle=1, size=6, mode=stat.S_IFREG | 0o644, links=1, flags=os.O_RDONLY):
    return dict(op='open', path=path, handle=handle, flags=flags,
                initial_size=size, initial_mode=mode, initial_nlink=links)


def read(offset, size, handle=1, path='/file', payload=0):
    return dict(op='read', path=path, handle=handle, offset=offset, bytes=size,
                object='payload.bin', payload_offset=payload)


class InitialReadTests(unittest.TestCase):
    def test_fully_observed_out_of_order_reads_are_planned(self):
        result = plan_initial_reads(events(opened(), read(3, 3, payload=7), read(0, 3)))
        self.assertEqual(result['/file']['size'], 6)
        self.assertEqual(result['/file']['segments'], [(0, 3, 0), (3, 3, 7)])

    def test_partial_and_missing_metadata_never_produce_a_plan(self):
        self.assertEqual(plan_initial_reads(events(opened(), read(0, 2), read(3, 3))), {})
        legacy = dict(op='open', path='/file', handle=1, flags=os.O_RDONLY)
        self.assertEqual(plan_initial_reads(events(legacy, read(0, 6))), {})
        self.assertEqual(plan_initial_reads(events(opened(links=2), read(0, 6))), {})

    def test_global_mutation_and_write_open_end_the_initial_window(self):
        for mutation in (dict(op='write', path='/other'), dict(op='rename', path='/x'),
                         opened(path='/other', handle=2, flags=os.O_WRONLY), dict(op='unknown')):
            self.assertEqual(plan_initial_reads(events(opened(), read(0, 3), mutation, read(3, 3))), {})
        self.assertIn('/file', plan_initial_reads(events(opened(), read(0, 6), dict(op='write', path='/other'))))

    def test_bad_ranges_duplicate_handles_and_truncation_are_rejected(self):
        self.assertEqual(plan_initial_reads(events(opened(), read(0, 7))), {})
        self.assertEqual(plan_initial_reads(events(opened(), read(0, 6, path='/other'))), {})
        with self.assertRaisesRegex(ValueError, 'handle'):
            plan_initial_reads(events(opened(), opened()))
        with self.assertRaisesRegex(ValueError, 'truncated'):
            plan_initial_reads(io.StringIO('{"sequence":1}'))
        with self.assertRaisesRegex(ValueError, 'sequence'):
            plan_initial_reads(io.StringIO('{"sequence":2,"op":"marker"}\n'))


if __name__ == '__main__':
    unittest.main()
