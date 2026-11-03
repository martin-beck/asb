"""Recorded direct/tool attribution must retain uncertainty."""
import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('fs_attribution', ROOT/'crates/asb-cli/assets/benchmarks/asb_fs_attribution.py')
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)


def origin(pid, tid, process_start, thread_start):
    return {'pid':pid, 'tid':tid, 'process_start_ticks':process_start, 'thread_start_ticks':thread_start}


def caller(tid, tgid, thread_start, ancestry, complete=True):
    return {'tid':tid, 'tgid':tgid, 'start_ticks':thread_start, 'ancestry':ancestry,
            'ancestry_complete':complete, 'thread_ancestry_complete':complete}


class AttributionTest(unittest.TestCase):
    def test_concurrent_siblings_and_subprocesses_are_not_mixed(self):
        a = module.Attribution()
        parent = {'tgid':100, 'start_ticks':11}
        first = origin(100, 110, 11, 21); second = origin(100, 120, 11, 22)
        self.assertEqual(a.observe({'sequence':1,'op':'marker','marker':'tool-begin:a','origin':first})['kind'], 'marker')
        a.observe({'sequence':2,'op':'marker','marker':'tool-begin:b','origin':second})
        direct = caller(130,100,23,[parent])
        self.assertEqual(a.observe({'sequence':3,'op':'read','caller':direct}), {'kind':'direct'})
        first_child = caller(200,200,31,[{'tgid':200,'start_ticks':31,'creator_thread':{'tid':110,'start_ticks':21}},parent])
        second_child = caller(300,300,32,[{'tgid':300,'start_ticks':32,'creator_thread':{'tid':120,'start_ticks':22}},parent])
        self.assertEqual(a.observe({'sequence':4,'op':'write','caller':first_child}), {'kind':'tool','alias':'a'})
        self.assertEqual(a.observe({'sequence':5,'op':'write','caller':second_child}), {'kind':'tool','alias':'b'})
        a.observe({'sequence':6,'op':'marker','marker':'tool-end:a'})
        a.observe({'sequence':7,'op':'marker','marker':'tool-end:b'})
        a.finish()

    def test_missing_lineage_and_overlapping_owners_never_guess(self):
        a = module.Attribution(); same = origin(100,110,11,21)
        a.observe({'sequence':1,'op':'marker','marker':'tool-begin:a','origin':same})
        a.observe({'sequence':2,'op':'marker','marker':'tool-begin:b','origin':same})
        current = caller(110,100,21,[{'tgid':100,'start_ticks':11}])
        self.assertEqual(a.observe({'sequence':3,'op':'read','caller':current}), {'kind':'ambiguous','aliases':['a','b']})
        self.assertEqual(a.observe({'sequence':4,'op':'release','caller':None}), {'kind':'unknown'})
        bad = caller(200,200,31,[{'tgid':200,'start_ticks':31,'creator_thread':None}, {'tgid':100,'start_ticks':11}])
        self.assertEqual(a.observe({'sequence':5,'op':'read','caller':bad}), {'kind':'unknown'})
        a.observe({'sequence':6,'op':'marker','marker':'tool-end:a'})
        a.observe({'sequence':7,'op':'marker','marker':'tool-end:b'})
        a.finish()

    def test_stale_generation_and_incomplete_marker(self):
        a = module.Attribution()
        a.observe({'sequence':1,'op':'marker','marker':'tool-begin:a','origin':origin(100,110,11,21)})
        stale = caller(110,100,99,[{'tgid':100,'start_ticks':11}])
        self.assertEqual(a.observe({'sequence':2,'op':'read','caller':stale}), {'kind':'direct'})
        a.observe({'sequence':3,'op':'marker','marker':'tool-end:a'})
        a.observe({'sequence':4,'op':'marker','marker':'tool-begin:b','origin':{'pid':100,'tid':110}})
        self.assertEqual(a.observe({'sequence':5,'op':'read','caller':stale}), {'kind':'unknown'})
        with self.assertRaisesRegex(ValueError,'did not settle'): a.finish()
        with self.assertRaisesRegex(ValueError,'sequence has a gap'): a.observe({'sequence':7,'op':'read'})

if __name__ == '__main__': unittest.main()
