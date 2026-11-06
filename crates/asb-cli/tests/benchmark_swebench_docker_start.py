"""Offline SWE-bench session startup failure protocol."""
import contextlib
import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))
import asb_docker
import asb_swebench


class DockerStartupTests(unittest.TestCase):
    def test_access_failure_reports_reason_and_not_created_disposition(self):
        class Unavailable:
            def __enter__(self):
                raise asb_docker.DockerProbeError("Docker access probe timed out")

            def __exit__(self, *_):
                raise AssertionError("failed entry must not be exited")

        output = io.StringIO()
        with patch.object(asb_docker, "DockerSession", Unavailable), \
             patch.object(asb_swebench, "install_session_cancel_handlers"), \
             contextlib.redirect_stdout(output):
            status = asb_swebench.main(["session", "--catalog", "catalog.json", "--task", "fixture", "--artifacts", "out"])
        self.assertEqual(status, 1)
        events = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(events, [
            {"event": "error", "error_class": "ExecutionError", "reason": "Docker access probe timed out"},
            {"event": "closed", "cleanup": "not_created"},
        ])

    def test_unsettled_transport_failure_is_not_reported_as_not_created(self):
        class Unsettled:
            def __enter__(self):
                raise asb_docker.DockerAccessError("transport exit is unconfirmed")

            def __exit__(self, *_):
                raise AssertionError("failed entry must not be exited")

        output = io.StringIO()
        with patch.object(asb_docker, "DockerSession", Unsettled), \
             patch.object(asb_swebench, "install_session_cancel_handlers"), \
             contextlib.redirect_stdout(output):
            with self.assertRaisesRegex(asb_docker.DockerAccessError, "unconfirmed"):
                asb_swebench.main(["session", "--catalog", "catalog.json", "--task", "fixture", "--artifacts", "out"])
        self.assertEqual(output.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
