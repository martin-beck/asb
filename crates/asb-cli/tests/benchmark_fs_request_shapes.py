"""The recorder retains request sizes even for short filesystem I/O."""
from contextlib import nullcontext
import hashlib
from pathlib import Path
import sys
import types
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
source = (ROOT/'crates/asb-cli/assets/benchmarks/asb_fs_proxy.py').read_text().rsplit('\nFUSE(Proxy()', 1)[0]
fuse = types.ModuleType('fuse')
fuse.FUSE = lambda *_args, **_kwargs: None
fuse.FuseOSError = OSError
fuse.Operations = type('Operations', (), {})
fuse.fuse_get_context = lambda: (0, 0, 0)
sys.modules['fuse'] = fuse
argv = sys.argv; sys.argv = ['fixture','/tmp/backing','/tmp/mount','/tmp/evidence']
namespace = {'__name__':'fs_request_fixture'}
try:
    exec(compile(source, 'asb_fs_proxy.py', 'exec'), namespace)
finally:
    sys.argv = argv

class RequestShapeTest(unittest.TestCase):
    def test_short_io_preserves_the_entire_request_shape(self):
        proxy = namespace['Proxy'].__new__(namespace['Proxy'])
        proxy.inode_lock = lambda _file: nullcontext()
        proxy.handle = lambda _file: 7
        events = []
        proxy.record = lambda op, path, data, **fields: events.append((op, path, data, fields))
        fh = types.SimpleNamespace(fh=5)
        with patch.object(namespace['os'], 'pread', return_value=b'ab'):
            self.assertEqual(proxy.read('/file', 10, 3, fh), b'ab')
        with patch.object(namespace['os'], 'pwrite', return_value=2):
            self.assertEqual(proxy.write('/file', b'abcdef', 3, fh), 2)
        read, write = events
        self.assertEqual(read[2], b'ab')
        self.assertEqual(read[3]['requested_size'], 10)
        self.assertEqual(write[2], b'ab')
        self.assertEqual(write[3]['requested_size'], 6)
        self.assertEqual(write[3]['request_sha256'], hashlib.sha256(b'abcdef').hexdigest())
        self.assertNotEqual(write[3]['request_sha256'], hashlib.sha256(b'ab').hexdigest())

if __name__ == '__main__': unittest.main()
