"""Offline profiling symbol publication checks with a tiny local ELF."""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('goose_profile_symbols', ROOT / 'crates/asb-cli/assets/scripts/goose-profile-symbols.py')
symbols = importlib.util.module_from_spec(spec)
spec.loader.exec_module(symbols)
SCRATCH = ROOT / 'target/goose-profile-symbol-tests'
SCRATCH.mkdir(parents=True, exist_ok=True)


@unittest.skipUnless(all(shutil.which(tool) for tool in ('cc', 'objcopy', 'readelf')), 'requires existing C compiler and binutils')
class ProfileSymbolsTest(unittest.TestCase):
    def test_compact_executable_and_debug_companion_share_build_id(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temporary:
            root = Path(temporary); source = root / 'fixture.c'; binary = root / 'binary'
            source.write_text('int main(void) { return 0; }\n')
            subprocess.run(['cc', '-g', '-Wl,--build-id', '-o', str(binary), str(source)], check=True)
            compact = symbols.prepare(binary, root / 'staging', root / 'symbols')
            identity = symbols.build_id(binary)
            debug = root / 'symbols' / '.build-id' / identity[:2] / (identity[2:] + '.debug')
            self.assertEqual(symbols.build_id(compact), identity)
            self.assertEqual(symbols.build_id(debug), identity)
            self.assertEqual(subprocess.run([str(compact)], check=False).returncode, 0)
            self.assertIn('.debug_info', subprocess.check_output(['readelf', '-SW', str(debug)], text=True))
            self.assertNotIn('.debug_info', subprocess.check_output(['readelf', '-SW', str(compact)], text=True))
            debug.write_bytes(b'conflicting evidence')
            with self.assertRaisesRegex(ValueError, 'conflict'):
                symbols.prepare(binary, root / 'staging', root / 'symbols')
            self.assertEqual(debug.read_bytes(), b'conflicting evidence')


if __name__ == '__main__':
    unittest.main()
