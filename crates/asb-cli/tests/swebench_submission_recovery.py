"""Offline regression for submissions after agent-owned Git metadata is removed."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import types
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
    def test_large_submission_patch_is_captured_and_limit_is_explained(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); worktree = root / 'worktree'; worktree.mkdir()
            def git(*args):
                return subprocess.run(['git', *args], cwd=worktree, check=True, capture_output=True).stdout
            git('init', '-q'); git('config', 'user.name', 'Fixture'); git('config', 'user.email', 'fixture@example.invalid')
            (worktree/'large.txt').write_text('before\n'); git('add','large.txt'); git('commit','-qm','base')
            base = git('rev-parse','HEAD').decode().strip()
            backup = root/'baseline.git'; shutil.copytree(worktree/'.git', backup)
            (worktree/'large.txt').write_text('large line for patch\n' * 450000)
            artifacts = root/'artifacts'; artifacts.mkdir()
            task = task_module.SweBenchTask(None,root/'catalog','fixture',artifacts)
            task.workspace = LocalWorkspace(worktree); task.workspace.image_identity = {'fixture':True}
            task.invocation = {'identity':{'task_id':'fixture'}}; task.row = {'base_commit':base}
            with patch.object(task_module,'SUBMISSION_GIT_DIR',str(backup)), patch.object(task_module,'SUBMISSION_WORKTREE',str(worktree)):
                submission = task.capture_submission()
                self.assertGreater(submission['patch_bytes'], 8_634_014)
                self.assertEqual((artifacts/'candidate.patch').stat().st_size, submission['patch_bytes'])
                self.assertEqual(len(asb_swebench.read_limited(artifacts/'candidate.patch', asb_swebench.MAX_PATCH_BYTES)), submission['patch_bytes'])
                with patch.object(task_module,'MAX_PATCH_BYTES',1024):
                    with self.assertRaisesRegex(task_module.ExecutionError, 'submission patch generation output exceeded the 1024 byte limit'):
                        task.capture_submission()

    def test_repository_gate_has_separate_preparation_and_execution_budgets(self):
        task = task_module.SweBenchTask(None, 'catalog', 'fixture', 'artifacts')
        task.workspace = MagicMock()
        task.workspace.execute.return_value = {
            'timed_out': False, 'cancelled': False, 'exit_code': 0,
            'output_truncated': False, 'stdout': b'ok',
        }
        task.preparing_repository = True
        self.assertEqual(task.checked(['git', 'status']), b'ok')
        options = task.workspace.execute.call_args.kwargs
        self.assertEqual(options['timeout'], 600)
        self.assertEqual(options['preparation_timeout'], 7200)
        task.checked(['git', 'status'], timeout=4)
        self.assertEqual(task.workspace.execute.call_args.kwargs['timeout'], 4)
        task.preparing_repository = False
        task.checked(['git', 'status'])
        options = task.workspace.execute.call_args.kwargs
        self.assertEqual(options['timeout'], 600)
        self.assertNotIn('preparation_timeout', options)
        task.checked(['id', '-g'])
        self.assertEqual(task.workspace.execute.call_args.kwargs['timeout'], 600)
        task.checked(['id', '-g'], timeout=4)
        self.assertEqual(task.workspace.execute.call_args.kwargs['timeout'], 4)
        budget = MagicMock()
        budget.remaining.return_value = 1234
        task.checked(['git', 'status'], timeout=60, preparation_budget=budget)
        options = task.workspace.execute.call_args.kwargs
        self.assertEqual(options['timeout'], 60)
        self.assertEqual(options['preparation_timeout'], 1234)
        budget.remaining.assert_called_once_with()
        ambient_budget = MagicMock()
        ambient_budget.remaining.return_value = 2345
        task._asb_preparation_budget = ambient_budget
        task.checked(['git', 'status'], timeout=60)
        options = task.workspace.execute.call_args.kwargs
        self.assertEqual(options['timeout'], 60)
        self.assertEqual(options['preparation_timeout'], 2345)
        ambient_budget.remaining.assert_called_once_with()

    def test_history_fetch_has_a_separate_bounded_setup_budget(self):
        task = task_module.SweBenchTask(None, 'catalog', 'fixture', 'artifacts')
        task.checked = MagicMock(return_value=b'')
        task.prepare_base_history('/worktree', '/base.git', 'a' * 40)
        fetches = [call for call in task.checked.call_args_list
                   if call.args[0][:3] == ['git', '--git-dir=/base.git', 'fetch']]
        self.assertEqual(len(fetches), 1)
        self.assertEqual(fetches[0].kwargs['timeout'], 7200)
        self.assertIn('--no-tags', fetches[0].args[0])
        self.assertIn('--no-write-fetch-head', fetches[0].args[0])

    def test_agent_environment_activates_sdk_environment_and_preserves_private_values(self):
        task = task_module.SweBenchTask(None, 'catalog', 'fixture', 'artifacts')
        task.row = {'base_commit':'a' * 40}
        calls = []
        def checked(argv, **options):
            calls.append((argv, options))
            if argv[0] == '/bin/bash':
                return b'/opt/miniconda3/envs/testbed/bin:/custom/bin\0/opt/miniconda3/envs/testbed\0testbed\0' + b'1\0'
            return b'1637276904\n'
        task.checked = checked
        sdk = types.ModuleType('swebench.image_builder.constants')
        sdk.CONTAINER_ENV_NAME = 'testbed'
        with patch.dict(sys.modules, {'swebench':types.ModuleType('swebench'),
                                     'swebench.image_builder':types.ModuleType('swebench.image_builder'),
                                     'swebench.image_builder.constants':sdk}):
            result = task.agent_environment({'PATH':'/custom/bin', 'HOME':'/private',
                                             'API_KEY':'secret', 'GIT_AUTHOR_DATE':'explicit'})
            self.assertEqual(result['PATH'], '/opt/miniconda3/envs/testbed/bin:/custom/bin')
            self.assertEqual(result['HOME'], '/private')
            self.assertEqual(result['API_KEY'], 'secret')
            self.assertEqual(result['GIT_AUTHOR_DATE'], 'explicit')
            self.assertEqual(result['GIT_COMMITTER_DATE'], '@1637276904 +0000')
            self.assertEqual(result['GIT_CONFIG_COUNT'], '1')
            self.assertEqual(result['GIT_CONFIG_KEY_0'], 'core.abbrev')
            self.assertEqual(result['GIT_CONFIG_VALUE_0'], '40')
            custom = task.agent_environment({'GIT_CONFIG_COUNT': '1', 'GIT_CONFIG_KEY_0': 'core.abbrev', 'GIT_CONFIG_VALUE_0': '12'})
            self.assertEqual(custom['GIT_CONFIG_VALUE_0'], '12')
            self.assertEqual(calls[0][1]['environment'], {'PATH':'/custom/bin'})
            calls.clear()
            budget = MagicMock()
            budget.remaining.return_value = 7200
            task.agent_environment({'PATH':'/custom/bin'}, preparation_budget=budget)
            self.assertEqual(len(calls), 2)
            self.assertTrue(all(call[1]['preparation_budget'] is budget for call in calls))
            task.checked = lambda *args, **kwargs: b'invalid'
            with self.assertRaises(task_module.ExecutionError):
                task.agent_environment({})

    def test_agent_environment_retries_only_read_only_probes(self):
        task = task_module.SweBenchTask(None, 'catalog', 'fixture', 'artifacts')
        task.row = {'base_commit':'a' * 40}
        calls = []
        def checked(argv, **options):
            calls.append(argv)
            if len(calls) == 1:
                raise task_module.ExecutionError('transient fixture failure')
            if argv[0] == '/bin/bash':
                return (b'/opt/miniconda3/envs/testbed/bin\0/opt/miniconda3/envs/testbed\0'
                        b'testbed\0' + b'1\0')
            return b'1637276904\n'
        task.checked = checked
        sdk = types.ModuleType('swebench.image_builder.constants')
        sdk.CONTAINER_ENV_NAME = 'testbed'
        with patch.dict(sys.modules, {'swebench':types.ModuleType('swebench'),
                                     'swebench.image_builder':types.ModuleType('swebench.image_builder'),
                                     'swebench.image_builder.constants':sdk}), \
                patch.object(task_module.time, 'sleep') as sleep:
            result = task.agent_environment({})
        self.assertEqual(result['CONDA_DEFAULT_ENV'], 'testbed')
        self.assertEqual([call[0] for call in calls], ['/bin/bash', '/bin/bash', 'git'])
        sleep.assert_called_once_with(0.1)

    def test_full_git_object_ids_preserve_distinct_commits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = dict(os.environ, GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='core.abbrev', GIT_CONFIG_VALUE_0='40',
                       GIT_AUTHOR_NAME='Fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid',
                       GIT_COMMITTER_NAME='Fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid')
            def git(*args):
                return subprocess.run(['git', *args], cwd=root, env=env, check=True, capture_output=True).stdout.decode().strip()
            git('init', '-q')
            for text in ('first', 'second'):
                (root/'file').write_text(text)
                git('add', 'file')
                git('commit', '-qm', text)
            ids = [line.split()[0] for line in git('log', '--oneline').splitlines()]
            self.assertEqual(len(ids), 2)
            self.assertTrue(all(len(value) == 40 for value in ids))
            self.assertNotEqual(ids[0], ids[1])

    def test_default_git_dates_stabilize_stash_without_masking_changed_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args, env=None):
                return subprocess.run(['git', *args], cwd=root, env=env, check=True,
                                      capture_output=True).stdout.strip().decode()
            git('init', '-q')
            git('config', 'user.name', 'Fixture')
            git('config', 'user.email', 'fixture@example.invalid')
            (root/'file').write_text('base\n')
            git('add', 'file')
            git('commit', '-qm', 'base')
            task = task_module.SweBenchTask(None, 'catalog', 'fixture', root/'artifacts')
            task.row = {'base_commit':git('rev-parse', 'HEAD')}
            task.workspace = LocalWorkspace(root)
            sdk = types.ModuleType('swebench.image_builder.constants')
            sdk.CONTAINER_ENV_NAME = 'testbed'
            actual_checked = task.checked
            def checked(argv, **kwargs):
                if argv[0] == '/bin/bash':
                    return b'/opt/miniconda3/envs/testbed/bin:/usr/bin:/bin\0/opt/miniconda3/envs/testbed\0testbed\0' + b'1\0'
                return actual_checked(argv, **kwargs)
            task.checked = checked
            with patch.dict(sys.modules, {'swebench':types.ModuleType('swebench'),
                                         'swebench.image_builder':types.ModuleType('swebench.image_builder'),
                                         'swebench.image_builder.constants':sdk}):
                environment = task.agent_environment(os.environ.copy())
            def stash(content):
                (root/'file').write_text(content)
                git('stash', 'push', '-q', env=environment)
                result = git('rev-parse', 'refs/stash')
                git('stash', 'drop', '-q', env=environment)
                return result
            first = stash('same\n')
            self.assertEqual(first, stash('same\n'))
            self.assertNotEqual(first, stash('different\n'))

    def test_replay_snapshot_scratch_is_attempt_scoped_and_recorded(self):
        scratch=Path(__file__).resolve().parents[3]/'target/snapshot-storage-tests'
        scratch.mkdir(parents=True,exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            root=Path(directory).resolve();artifacts=root/'artifacts';artifacts.mkdir();storage_root=root/'scratch';storage_root.mkdir()
            attempt='asb-'+'a'*32
            with patch.dict(os.environ,{'ASB_EXPERIMENTAL_SNAPSHOT_STORAGE_ROOT':str(storage_root)}):
                storage=task_module.snapshot_storage_path(artifacts,attempt,'replay')
                self.assertEqual(storage,storage_root/attempt)
                location=json.loads((artifacts/'snapshot-storage-location.json').read_text())
                self.assertEqual(location['path'],str(storage));self.assertEqual(location['attempt'],attempt)
                self.assertEqual(location['scope'],'experimental-retained-replay-scratch')
                with self.assertRaises(FileExistsError):task_module.snapshot_storage_path(artifacts,attempt,'replay')
            self.assertTrue(storage.is_dir())

    def test_recording_uses_project_storage_even_with_replay_scratch_configured(self):
        scratch=Path(__file__).resolve().parents[3]/'target/snapshot-storage-tests'
        scratch.mkdir(parents=True,exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            root=Path(directory).resolve();artifacts=root/'artifacts';artifacts.mkdir()
            with patch.dict(os.environ,{'ASB_EXPERIMENTAL_SNAPSHOT_STORAGE_ROOT':str(root/'missing')}):
                storage=task_module.snapshot_storage_path(artifacts,'asb-'+'b'*32,'record')
            self.assertEqual(storage,artifacts/'snapshot-storage');self.assertTrue(storage.is_dir())
            self.assertFalse((artifacts/'snapshot-storage-location.json').exists())

    def test_replay_rejects_invalid_scratch_root_without_allocating_storage(self):
        scratch=Path(__file__).resolve().parents[3]/'target/snapshot-storage-tests'
        scratch.mkdir(parents=True,exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            artifacts=Path(directory)/'artifacts';artifacts.mkdir()
            for value in ('relative',str(Path(directory)/'missing')):
                with self.subTest(value=value),patch.dict(os.environ,{'ASB_EXPERIMENTAL_SNAPSHOT_STORAGE_ROOT':value}):
                    with self.assertRaisesRegex(task_module.ExecutionError,'existing absolute directory'):
                        task_module.snapshot_storage_path(artifacts,'asb-'+'c'*32,'replay')
            self.assertEqual(list(artifacts.iterdir()),[])

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

    def test_session_reads_optional_validated_cpu_allocation(self):
        with patch.dict(os.environ, {"ASB_CPU_ALLOCATION": "5,1-3"}):
            task = task_module.SweBenchTask(None, "catalog", "fixture", "artifacts")
            self.assertEqual(task.cpu_set, [1, 2, 3, 5])
        with patch.dict(os.environ, {"ASB_CPU_ALLOCATION": "1,1"}):
            with self.assertRaises(task_module.ExecutionError):
                task_module.SweBenchTask(None, "catalog", "fixture", "artifacts")
        with patch.dict(os.environ, {}, clear=True):
            self.assertIsNone(task_module.SweBenchTask(None, "catalog", "fixture", "artifacts").cpu_set)

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
