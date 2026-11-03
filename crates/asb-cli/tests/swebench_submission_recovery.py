"""Offline regression for submissions after agent-owned Git metadata is removed."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parents[1] / "assets" / "benchmarks"))
import asb_swebench_task as task_module
import asb_swebench


class LocalWorkspace:
    def __init__(self, worktree):
        self.worktree = worktree

    def execute(self, argv, environment=None, timeout=60, output_limit=256 * 1024, cancelled=None):
        result = subprocess.run(argv, cwd=self.worktree, capture_output=True, timeout=timeout, env=environment)
        return {"timed_out": False, "cancelled": False, "exit_code": result.returncode,
                "output_truncated": len(result.stdout) > output_limit, "stdout": result.stdout}


class SubmissionRecoveryTests(unittest.TestCase):
    def test_task_preserves_tool_mode(self):
        for mode in ('none','record','replay'):
            self.assertEqual(task_module.SweBenchTask(None,'catalog','fixture','artifacts',tool_mode=mode).tool_mode,mode)
            self.assertEqual(task_module.SweBenchTask(None,'catalog','fixture','artifacts',tool_mode=mode,snapshot=True).snapshot,mode!='none')
        with self.assertRaisesRegex(ValueError,'unsupported workload tool mode'):
            task_module.SweBenchTask(None,'catalog','fixture','artifacts',tool_mode='unexpected')

    def test_session_entrypoint_forwards_tool_mode(self):
        for mode in ('none','record','replay'):
            with self.subTest(mode=mode), patch.dict(os.environ,{'ASB_WORKLOAD_TOOL_MODE':mode}), \
                 patch('asb_docker.DockerSession') as docker, patch('asb_task_server.serve') as serve, \
                 patch('asb_swebench.signal.signal'):
                docker.return_value.__enter__.return_value=MagicMock()
                serve.side_effect=lambda task, *_: task.tool_mode
                result=asb_swebench.main(['session','--catalog','catalog','--task','fixture','--artifacts','artifacts'])
                self.assertEqual(result,mode)
    def test_preserved_baseline_captures_changes_after_git_metadata_deleted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            worktree = root / "worktree"
            worktree.mkdir()
            def git(*args):
                return subprocess.run(["git", *args], cwd=worktree, check=True, capture_output=True).stdout
            git("init", "-q")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "fixture@example.invalid")
            (worktree / "existing.txt").write_text("before\n")
            git("add", "existing.txt")
            git("commit", "-qm", "base")
            git("gc", "--prune=now")
            base = git("rev-parse", "HEAD").decode().strip()
            backup = root / "baseline.git"
            shutil.copytree(worktree / ".git", backup)
            (worktree / "existing.txt").write_text("after\n")
            (worktree / "new.txt").write_text("added\n")
            shutil.rmtree(worktree / ".git")
            artifacts = root / "artifacts"
            artifacts.mkdir()
            task = task_module.SweBenchTask(None, root / "catalog.json", "fixture", artifacts)
            task.workspace = LocalWorkspace(worktree)
            task.invocation = {"identity": {"task_id": "fixture"}}
            task.row = {"base_commit": base}
            task.workspace.image_identity = {"fixture": True}
            original = task_module.SUBMISSION_GIT_DIR, task_module.SUBMISSION_WORKTREE
            task_module.SUBMISSION_GIT_DIR, task_module.SUBMISSION_WORKTREE = str(backup), str(worktree)
            try:
                submission = task.capture_submission()
                patch = (artifacts / submission["artifact"]).read_text()
                self.assertIn("-before", patch)
                self.assertIn("+after", patch)
                self.assertIn("+added", patch)
                self.assertFalse((worktree / ".git").exists())
                self.assertEqual(json.loads((artifacts / "submission.json").read_text())["patch_bytes"], len(patch.encode()))
                shutil.rmtree(backup)
                with self.assertRaises(task_module.ExecutionError):
                    task.capture_submission()
            finally:
                task_module.SUBMISSION_GIT_DIR, task_module.SUBMISSION_WORKTREE = original


if __name__ == "__main__":
    unittest.main()
