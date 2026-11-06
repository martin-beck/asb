"""Opt-in actual SWE-bench prepared-material qualification, without an LLM.

Run with the installed SWE-bench SDK interpreter and ASB_TEST_SWECATALOG_ROOT
pointing at an existing benchmark project. Only newly owned fixture containers
are prepared; their normal teardown does not remove shared images or caches.
Artifacts remain in the benchmark project. No agent or evaluator is launched.
"""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))


@unittest.skipUnless(os.environ.get("ASB_TEST_SWECATALOG_ROOT"),
                     "requires explicit Docker and installed SWE-bench fixture")
class PreparedMaterialQualification(unittest.TestCase):
    def test_original_swe_preparation_is_repeatable_and_rejects_dirty_material(self):
        from asb_docker import DockerSession
        from asb_execution import ExecutionError
        from asb_material import tar_digest
        from asb_swebench_task import SweBenchTask
        root = Path(os.environ["ASB_TEST_SWECATALOG_ROOT"]).absolute()
        artifacts = Path(tempfile.mkdtemp(prefix="prepared-material-", dir=root))
        print(f"Retained prepared-material qualification: {artifacts}", flush=True)
        identities = []
        with DockerSession() as client:
            for index in range(2):
                directory = artifacts / f"attempt-{index + 1}"
                with SweBenchTask(client, root / "workloads/swe-bench-lite/catalog.json",
                                  "astropy__astropy-12907", directory) as task:
                    if index == 0:
                        chunks,metadata = task.workspace.container.get_archive(
                            "/",chunk_size=512,encode_stream=False)
                        try:
                            self.assertEqual(metadata["name"],"/")
                            raw = client.api._get_raw_response_socket(chunks.response)
                            self.assertEqual(getattr(raw,"_sock",raw).gettimeout(),600.0)
                            self.assertEqual(len(tar_digest(chunks,"/",root_metadata_only=True,
                                expected_root_kind=b"5")),64)
                            self.assertTrue(chunks.closed)
                        finally:
                            chunks.close()
                    binary = os.environ.get("ASB_TEST_BENCHMARK_ASB")
                    first = task.initial_material(binary=binary)
                    self.assertEqual(first, task.initial_material(binary=binary))
                    self.assertEqual(first["scope"], "prepared-workload-material-v1")
                    identities.append(first)
                    if index == 0:
                        # Intentional mutation is confined to this new task.
                        task.checked(["git", "config", "asb.materialQualification", "changed"])
                        self.assertNotEqual(first, task.initial_material(binary=binary))
                        task.checked(["git", "config", "--unset", "asb.materialQualification"])
                        task.checked(["touch", "asb-material-qualification-untracked.txt"])
                        with self.assertRaises(ExecutionError):
                            task.initial_material(binary=binary)
                cleanup = json.loads((directory / "task-cleanup.json").read_bytes())
                self.assertEqual(cleanup["status"], "removed")
        self.assertEqual(identities[0], identities[1])
        (artifacts / "qualification.json").write_text(json.dumps({
            "scope": "two-original-swe-preparations-not-full-population",
            "identities": identities, "agent_started": False,
            "provider_called": False, "evaluator_started": False,
            "root_helper":bool(os.environ.get("ASB_TEST_BENCHMARK_ASB")),
        }, sort_keys=True) + "\n")


if __name__ == "__main__":
    unittest.main()
