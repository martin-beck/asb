"""Experimental metadata recording preserves outcomes and remains opt-in."""
import json
import os
from pathlib import Path
import runpy
import sys
import tempfile
import types
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / 'crates/asb-cli/assets/benchmarks/asb_fs_proxy.py'
sys.path.insert(0, str(SCRIPT.parent))


class MetadataObservationTest(unittest.TestCase):
    def test_success_failure_and_opt_out(self):
        with tempfile.TemporaryDirectory(dir=ROOT / 'target') as directory:
            root = Path(directory)
            backing, mount, evidence = (root / name for name in ('backing', 'mount', 'evidence'))
            for path in (backing, mount, evidence): path.mkdir()
            (backing / 'file').write_bytes(b'contents')
            (backing / 'link').symlink_to('file')
            fake = types.ModuleType('fuse')
            fake.Operations = object
            fake.FuseOSError = OSError
            fake.fuse_get_context = lambda: (os.getuid(), os.getgid(), os.getpid())
            instances = []
            fake.FUSE = lambda proxy, *_args, **_kw: instances.append(proxy)
            with patch.dict(sys.modules, {'fuse': fake}), patch.object(sys, 'argv', [str(SCRIPT), str(backing), str(mount), str(evidence)]), patch.dict(os.environ, {'ASB_PROBE_ROOT_TGID': str(os.getpid()), 'ASB_PROBE_METADATA_CAPTURE': '1'}):
                runpy.run_path(str(SCRIPT), run_name='__main__')
                proxy = instances[0]
                metadata = proxy.getattr('/file')
                names = proxy.readdir('/', None)
                self.assertEqual(proxy.readlink('/link'), 'file')
                with self.assertRaises(OSError): proxy.getattr('/missing')
                with patch.dict(os.environ, {'ASB_PROBE_METADATA_CAPTURE': '0'}):
                    self.assertEqual(proxy.getattr('/file')['st_size'], 8)
                proxy.log.close(); proxy.payload.file.close()
            entries = [json.loads(line) for line in (evidence / 'events.jsonl').read_text().splitlines()]
            self.assertEqual([item['op'] for item in entries], ['observe-getattr', 'observe-readdir', 'observe-readlink', 'observe-getattr'])
            self.assertEqual(entries[0]['result'], metadata)
            self.assertEqual(entries[1]['result'], names)
            self.assertEqual(entries[2]['result'], 'file')
            self.assertEqual(entries[3]['error'], 2)

    def test_proxy_returns_recorded_metadata_and_respects_write_boundary(self):
        with tempfile.TemporaryDirectory(dir=ROOT/'target') as directory:
            root=Path(directory); backing=root/'backing'; mount=root/'mount'; evidence=root/'evidence'; source=root/'source'
            for path in (backing,mount,evidence,source): path.mkdir()
            (backing/'file').write_bytes(b'actual bytes')
            (source/'payload.bin').write_bytes(b'')
            def attrs(size):
                return dict(st_gid=1,st_ino=1,st_mode=33188,st_nlink=1,st_size=size,st_uid=1,
                            st_blocks=1,st_blksize=4096,st_atime=1,st_mtime=1,st_ctime=1)
            events=[{'op':'observe-getattr','path':'/file','result':attrs(3)},
                    {'op':'write','path':'/file'},
                    {'op':'observe-getattr','path':'/file','result':attrs(4)},
                    {'op':'marker','path':None,'marker':'tool-begin:first'}]
            (source/'events.jsonl').write_text(''.join(json.dumps(event)+'\n' for event in events))
            fake=types.ModuleType('fuse');fake.Operations=object;fake.FuseOSError=OSError
            fake.fuse_get_context=lambda:(os.getuid(),os.getgid(),os.getpid())
            instances=[];fake.FUSE=lambda proxy,*_args,**_kw:instances.append(proxy)
            with patch.dict(sys.modules,{'fuse':fake}), patch.object(sys,'argv',[str(SCRIPT),str(backing),str(mount),str(evidence)]), patch.dict(os.environ,{'ASB_PROBE_ROOT_TGID':str(os.getpid()),'ASB_PROBE_METADATA_REPLAY':str(source)}):
                runpy.run_path(str(SCRIPT),run_name='__main__');proxy=instances[0]
                self.assertEqual(proxy.getattr('/file')['st_size'],3)
                with self.assertRaisesRegex(RuntimeError,'no recorded'):
                    proxy.getattr('/file')
                proxy.record('write','/file')
                self.assertEqual(proxy.getattr('/file')['st_size'],4)
                proxy.log.close();proxy.payload.file.close()
            self.assertEqual((backing/'file').read_bytes(),b'actual bytes')


if __name__ == '__main__':
    unittest.main()
