"""Offline tests for the experimental Goose source adapter."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT / 'crates/asb-cli/assets/scripts/goose-native-prepare.py'
spec = importlib.util.spec_from_file_location('goose_native_prepare', SOURCE)
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)
SCRATCH = ROOT / 'target/goose-native-prepare-tests'
SCRATCH.mkdir(parents=True, exist_ok=True)


class GooseNativePrepareTest(unittest.TestCase):
    def setup_source(self, root, version='1.54.0'):
        (root / 'Cargo.toml').write_text(f'[workspace.package]\nversion = "{version}"\n')
        patch_file = root / 'adapter.patch'
        patch_file.write_text('fixture')
        return patch_file

    def test_pinned_source_checks_before_applying(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory); patch_file = self.setup_source(root)
            with patch.object(prepare, 'SOURCE_HASHES', {}), patch.object(prepare.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
                prepare.prepare(root, patch_file)
            self.assertEqual([call.args[0][1:4] for call in run.call_args_list], [['apply', '--unidiff-zero', '--check'], ['apply', '--unidiff-zero', str(patch_file.resolve())]])
            self.assertTrue(all(call.kwargs['cwd'] == root.resolve() for call in run.call_args_list))

    def test_changed_layout_never_runs_apply(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory); patch_file = self.setup_source(root)
            with patch.object(prepare, 'SOURCE_HASHES', {}), patch.object(prepare.subprocess, 'run', return_value=subprocess.CompletedProcess([], 1)) as run:
                with self.assertRaisesRegex(ValueError, 'does not match'):
                    prepare.prepare(root, patch_file)
            self.assertEqual(run.call_count, 1)

    def test_real_git_applies_inside_parent_checkout_and_rejects_repeat(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory); patch_file = self.setup_source(root)
            (root / 'fixture.txt').write_text('before\n')
            patch_file.write_text('diff --git a/fixture.txt b/fixture.txt\n--- a/fixture.txt\n+++ b/fixture.txt\n@@ -1 +1 @@\n-before\n+after\n')
            with patch.object(prepare, 'SOURCE_HASHES', {}):
                prepare.prepare(root, patch_file)
                self.assertEqual((root / 'fixture.txt').read_text(), 'after\n')
                with self.assertRaisesRegex(ValueError, 'already patched'):
                    prepare.prepare(root, patch_file)

    def test_new_version_is_rejected_before_any_patch(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory); patch_file = self.setup_source(root, '1.55.0')
            with patch.object(prepare.subprocess, 'run') as run:
                with self.assertRaisesRegex(ValueError, 'expected 1.54.0'):
                    prepare.prepare(root, patch_file)
            run.assert_not_called()

    def test_same_version_with_modified_dispatch_is_rejected_before_git(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory); patch_file = self.setup_source(root)
            (root / 'dispatch.rs').write_text('modified')
            with patch.object(prepare, 'SOURCE_HASHES', {'dispatch.rs': '0' * 64}), patch.object(prepare.subprocess, 'run') as run:
                with self.assertRaisesRegex(ValueError, 'does not match'):
                    prepare.prepare(root, patch_file)
            run.assert_not_called()

    def test_missing_workspace_is_actionable(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            with self.assertRaisesRegex(ValueError, 'workspace version'):
                prepare.prepare(directory, Path(directory) / 'missing.patch')


if __name__ == '__main__':
    unittest.main()
