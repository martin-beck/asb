"""Offline coverage for conservative initial-read planning."""
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, str(Path(__file__).parents[1] / 'assets/benchmarks'))
from asb_fs_initial_reads import plan_initial_reads, restore_initial_reads


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
    def test_four_workspaces_prepare_independent_initial_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); evidence = root / 'evidence'; evidence.mkdir()
            workspaces = []
            for index in range(4):
                workspace = root / f'workspace-{index}'; workspace.mkdir()
                (workspace / 'file').write_bytes(b'xxxxxx')
                workspaces.append(workspace)
            mode = (workspaces[0] / 'file').stat().st_mode
            (evidence / 'payload.bin').write_bytes(b'abcdef')
            (evidence / 'events.jsonl').write_text(events(opened(mode=mode), read(0, 6)).getvalue())
            with ThreadPoolExecutor(max_workers=4) as pool:
                self.assertEqual(list(pool.map(lambda workspace: restore_initial_reads(
                    evidence, workspace, evidence), workspaces)), [1, 1, 1, 1])
            for workspace in workspaces:
                self.assertEqual((workspace / 'file').read_bytes(), b'abcdef')
            (workspaces[0] / 'file').write_bytes(b'changed')
            self.assertEqual((workspaces[1] / 'file').read_bytes(), b'abcdef')

    def test_restore_verifies_overlaps_and_preserves_inode_and_mtime(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); evidence = root / 'evidence'; evidence.mkdir()
            workspace = root / 'workspace'; workspace.mkdir()
            target = workspace / 'file'; target.write_bytes(b'xxxxxx')
            before = target.stat()
            (evidence / 'payload.bin').write_bytes(b'abcdefcde')
            (evidence / 'events.jsonl').write_text(events(opened(mode=before.st_mode),
                read(0, 6), read(2, 3, payload=6)).getvalue())
            self.assertEqual(restore_initial_reads(evidence, workspace, evidence), 1)
            self.assertEqual(target.read_bytes(), b'abcdef')
            after = target.stat()
            self.assertEqual(after.st_ino, before.st_ino)
            self.assertEqual(after.st_mtime_ns, before.st_mtime_ns)

    def test_conflicting_or_truncated_payload_never_changes_destination(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); evidence = root / 'evidence'; evidence.mkdir()
            workspace = root / 'workspace'; workspace.mkdir()
            target = workspace / 'file'; target.write_bytes(b'xxxxxx')
            before = target.stat()
            (evidence / 'events.jsonl').write_text(events(opened(mode=before.st_mode),
                read(0, 6), read(2, 3, payload=6)).getvalue())
            for payload, message in [(b'abcdefXYZ', 'overlapping'), (b'abc', 'truncated')]:
                (evidence / 'payload.bin').write_bytes(payload)
                with self.assertRaisesRegex(ValueError, message):
                    restore_initial_reads(evidence, workspace, evidence)
                self.assertEqual(target.read_bytes(), b'xxxxxx')

    def test_compact_digest_is_checked_before_initial_state_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); evidence = root/'evidence'; evidence.mkdir()
            workspace = root/'workspace'; workspace.mkdir(); target = workspace/'file'; target.write_bytes(b'xxxxxx')
            mode = target.stat().st_mode; (evidence/'payload.bin').write_bytes(b'abcdef')
            digest = dict(op='read', path='/file', handle=1, offset=1, bytes=3, requested_size=3,
                          payload_sha256=hashlib.sha256(b'bcd').hexdigest())
            (evidence/'events.jsonl').write_text(events(opened(mode=mode), read(0,6), digest).getvalue())
            self.assertEqual(restore_initial_reads(evidence,workspace,evidence),1)
            self.assertEqual(target.read_bytes(),b'abcdef')
            target.write_bytes(b'xxxxxx'); digest['payload_sha256']=hashlib.sha256(b'bad').hexdigest()
            (evidence/'events.jsonl').write_text(events(opened(mode=mode), read(0,6), digest).getvalue())
            with self.assertRaisesRegex(ValueError,'digest differs'):
                restore_initial_reads(evidence,workspace,evidence)
            self.assertEqual(target.read_bytes(),b'xxxxxx')

    def test_all_destinations_are_checked_before_any_write(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); evidence = root / 'evidence'; evidence.mkdir()
            workspace = root / 'workspace'; workspace.mkdir()
            target = workspace / 'file'; target.write_bytes(b'xxxxxx')
            mode = target.stat().st_mode
            (evidence / 'payload.bin').write_bytes(b'abcdefz')
            (evidence / 'events.jsonl').write_text(events(opened(mode=mode), read(0, 6),
                opened(path='/missing', handle=2, size=1, mode=mode),
                read(0, 1, handle=2, path='/missing', payload=6)).getvalue())
            with self.assertRaises(FileNotFoundError):
                restore_initial_reads(evidence, workspace, evidence)
            self.assertEqual(target.read_bytes(), b'xxxxxx')

    def test_fully_observed_out_of_order_reads_are_planned(self):
        result = plan_initial_reads(events(opened(), read(3, 3, payload=7), read(0, 3)))
        self.assertEqual(result['/file']['size'], 6)
        self.assertEqual(result['/file']['segments'], [(0, 3, 0), (3, 3, 7)])

    def test_metadata_observations_do_not_end_initial_read_window(self):
        metadata = dict(op='observe-getattr', path='/file', result={'st_size':6})
        self.assertIn('/file', plan_initial_reads(events(metadata, opened(), read(0, 6))))
        unknown = dict(op='observe-unknown', path='/file')
        self.assertEqual(plan_initial_reads(events(unknown, opened(), read(0, 6))), {})

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
