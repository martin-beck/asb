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
    def test_snapshot_baseline_uses_attempt_storage(self):
        ordinary=task_module.SweBenchTask(None,'catalog','fixture','artifacts')
        snapshot=task_module.SweBenchTask(None,'catalog','fixture','artifacts',tool_mode='replay',snapshot=True)
        self.assertEqual(ordinary.submission_git_dir(),task_module.SUBMISSION_GIT_DIR)
        self.assertEqual(snapshot.submission_git_dir(),'/asb-snapshot-storage/baseline.git')

    def test_reflog_digest_ignores_cross_ref_order_but_binds_each_entry(self):
        first = b'HEAD@{0}\x00commit\x00reset\x00actor\x00email\x00\n'
        second = b'refs/heads/base@{0}\x00commit\x00create\x00actor\x00email\x00\n'
        self.assertEqual(task_module.reflog_digest(first + second), task_module.reflog_digest(second + first))
        self.assertNotEqual(task_module.reflog_digest(first + second), task_module.reflog_digest(first + second.replace(b'actor', b'other')))
        self.assertNotEqual(task_module.reflog_digest(first + second), task_module.reflog_digest(first))

    def test_task_history_keeps_ancestors_and_excludes_later_commit_objects(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);worktree=root/'worktree';worktree.mkdir()
            def git(*args,check=True):
                return subprocess.run(['git',*args],cwd=worktree,check=check,capture_output=True)
            git('init','-q');git('config','user.name','Fixture');git('config','user.email','fixture@example.invalid')
            (worktree/'source').write_text('ancestor\n');git('add','source');git('commit','-qm','ancestor')
            ancestor=git('rev-parse','HEAD').stdout.decode().strip()
            (worktree/'source').write_text('base\n');git('commit','-qam','base')
            base=git('rev-parse','HEAD').stdout.decode().strip()
            (worktree/'source').write_text('later fix\n');git('commit','-qam','future fix')
            future=git('rev-parse','HEAD').stdout.decode().strip()
            git('reset','--hard',base)
            task=task_module.SweBenchTask(None,root/'catalog','fixture',root/'artifacts')
            task.workspace=LocalWorkspace(worktree)
            task.prepare_base_history(str(worktree),str(root/'fresh.git'),base)
            self.assertEqual(git('rev-parse','HEAD').stdout.decode().strip(),base)
            self.assertEqual(git('status','--porcelain').stdout,b'')
            actors=git('reflog','show','--all','--format=%gn <%ge>').stdout.decode().splitlines()
            self.assertTrue(actors)
            self.assertEqual(set(actors),{'ASB workload preparation <asb-workload@localhost>'})
            self.assertEqual(git('cat-file','-e',ancestor+'^{commit}',check=False).returncode,0)
            self.assertNotEqual(git('cat-file','-e',future+'^{commit}',check=False).returncode,0)
            self.assertFalse((root/'fresh.git.unfiltered').exists())

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
