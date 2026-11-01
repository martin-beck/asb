#!/usr/bin/env python3
"""Unit tests for the path-to-test ownership table."""

import unittest
from pathlib import Path

from ci_pr_tests import component_targets


class ComponentTargetTests(unittest.TestCase):
    def test_ci_concurrency_keeps_active_and_separates_pr_ref_and_event(self):
        workflow = (Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml").read_text()
        self.assertIn("  cancel-in-progress: false\n", workflow)
        self.assertIn("github.event_name == 'pull_request' && format('pr-{0}', github.event.pull_request.number)", workflow)
        self.assertIn("format('{0}-{1}', github.event_name, github.ref)", workflow)
        self.assertIn("group: ${{ github.workflow }}-", workflow)

    def test_adapter_source_uses_owning_component(self):
        self.assertEqual(
            component_targets(["crates/asb-cli/src/agents/adapters/pi.rs"]),
            ("lib:agents::",),
        )

    def test_named_integration_test_is_selected_directly(self):
        self.assertEqual(
            component_targets(["crates/asb-cli/tests/offline_cli.rs"]),
            ("test:offline_cli",),
        )

    def test_runner_facade_uses_bounded_direct_unit_tests(self):
        self.assertEqual(
            component_targets(["crates/asb-cli/src/runner.rs"]),
            ("lib:runner::tests::",),
        )

    def test_shared_source_keeps_cross_command_smoke_coverage(self):
        self.assertEqual(
            component_targets(["crates/asb-cli/src/lib.rs"]),
            ("lib:config::", "lib:parser::", "lib:project::"),
        )

    def test_documentation_needs_no_rust_test_target(self):
        self.assertEqual(component_targets(["docs/DEVELOPMENT.md"]), ())


if __name__ == "__main__":
    unittest.main()
