"""Offline tests for ASB's internal Goose source build step."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('goose_native_build', ROOT / 'crates/asb-cli/assets/scripts/goose-native-build.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)
SCRATCH = ROOT / 'target/goose-native-build-tests'
SCRATCH.mkdir(parents=True, exist_ok=True)


class GooseNativeBuildTest(unittest.TestCase):
    def test_builds_default_features_and_publishes_executable(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory); source = root / 'source'; source.mkdir()
            binary = root / 'target/release/goose'; binary.parent.mkdir(parents=True); binary.write_bytes(b'fixture binary')
            output = root / 'tools/goose/bin/goose'
            def run(args, **kwargs):
                return subprocess.CompletedProcess(args, 73 if args[0] == str(binary) else 0)
            with patch.object(builder.prepare, 'prepare') as prepare, patch.object(builder.subprocess, 'run', side_effect=run) as invoked:
                builder.build(source, root / 'target', output, offline=True)
            self.assertEqual(output.read_bytes(), b'fixture binary')
            self.assertEqual(output.stat().st_mode & 0o777, 0o755)
            prepare.assert_called_once()
            build = invoked.call_args_list[0]
            self.assertIn('--offline', build.args[0])
            self.assertIn('asb-native-probe', build.args[0])
            self.assertNotIn('--no-default-features', build.args[0])
            self.assertEqual(build.kwargs['env']['CARGO_TARGET_DIR'], str((root / 'target').resolve()))
            self.assertNotIn('RUSTUP_TOOLCHAIN', build.kwargs['env'])

    def test_failed_capability_probe_preserves_existing_install(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory); source = root / 'source'; source.mkdir()
            output = root / 'goose'; output.write_bytes(b'existing installation')
            with patch.object(builder.prepare, 'prepare'), patch.object(builder.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)):
                with self.assertRaisesRegex(ValueError, 'did not provide'):
                    builder.build(source, root / 'target', output)
            self.assertEqual(output.read_bytes(), b'existing installation')


if __name__ == '__main__':
    unittest.main()
