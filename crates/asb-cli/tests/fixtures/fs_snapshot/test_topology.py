"""Offline tests for the experimental snapshot hard-link split planner."""
import os
from pathlib import Path
import tempfile
import unittest

from topology import detach_splits, plan_splits, regular_files

SCRATCH = Path(__file__).resolve().parents[5] / 'target' / 'snapshot-topology-tests'
SCRATCH.mkdir(parents=True, exist_ok=True)


class TopologyTest(unittest.TestCase):
    def test_plans_arbitrary_splits_and_keeps_original_open_inode(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);(root/'a').write_bytes(b'old');os.link(root/'a',root/'b');os.link(root/'a',root/'c')
            replica=regular_files(root)
            before={'a':(1,10),'b':(1,10),'c':(1,10)}
            after={'a':(2,10),'b':(2,20),'c':(2,30)}
            plan=plan_splits(before,after,replica)
            self.assertEqual(plan,['b','c'])
            fd=os.open(root/'b',os.O_RDONLY)
            try:
                detach_splits(root,plan)
                (root/'b').write_bytes(b'new')
                self.assertEqual(os.pread(fd,3,0),b'old')
                self.assertEqual((root/'a').read_bytes(),b'old')
                self.assertNotEqual((root/'a').stat().st_ino,(root/'b').stat().st_ino)
            finally:
                os.close(fd)

    def test_in_place_change_and_new_or_deleted_paths_need_no_detach(self):
        before={'a':(1,10),'b':(1,10)}
        replica={'a':(7,30),'b':(7,30)}
        self.assertEqual(plan_splits(before,{'a':(2,10),'c':(2,40)},replica),[])

    def test_hardlink_join_detaches_only_replaced_path(self):
        before={'a':(1,10),'b':(1,20),'c':(1,30)}
        after={'a':(2,10),'b':(2,10),'c':(2,30)}
        replica={'a':(7,40),'b':(7,50),'c':(7,60)}
        self.assertEqual(plan_splits(before,after,replica),['b'])

    def test_many_to_one_replica_alias_is_rejected(self):
        before={'a':(1,10),'b':(1,20)}
        with self.assertRaisesRegex(ValueError,'topology differs'):
            plan_splits(before,before,{'a':(7,30),'b':(7,30)})

    def test_rejects_mismatched_replica_and_unhandled_inode_move(self):
        before={'a':(1,10),'b':(1,10)}
        with self.assertRaisesRegex(ValueError,'paths differ'):
            plan_splits(before,{}, {'a':(7,30)})
        with self.assertRaisesRegex(ValueError,'topology differs'):
            plan_splits(before,{}, {'a':(7,30),'b':(7,31)})
        with self.assertRaisesRegex(ValueError,'rename handling'):
            plan_splits(before,{'c':(2,10)}, {'a':(7,30),'b':(7,30)})

    def test_unlinked_symlinks_are_not_regular_file_inventory(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);(root/'file').write_bytes(b'x');(root/'link').symlink_to('file')
            self.assertEqual(set(regular_files(root)),{'file'})


if __name__=='__main__':unittest.main()
