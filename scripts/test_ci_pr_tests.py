#!/usr/bin/env python3
"""Unit tests for the path-to-test ownership table."""

import unittest

from ci_pr_tests import component_targets


class ComponentTargetTests(unittest.TestCase):
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

    def test_shared_source_keeps_cross_command_smoke_coverage(self):
        self.assertEqual(
            component_targets(["crates/asb-cli/src/lib.rs"]),
            ("lib:config::", "lib:parser::", "lib:project::"),
        )

    def test_documentation_needs_no_rust_test_target(self):
        self.assertEqual(component_targets(["docs/DEVELOPMENT.md"]), ())


if __name__ == "__main__":
    unittest.main()
