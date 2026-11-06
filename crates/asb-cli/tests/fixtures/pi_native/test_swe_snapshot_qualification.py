"""Offline checks for the Pi/SWE qualification's host-gate observation."""
import unittest

from swe_snapshot_c4 import check_gate_diagnostics


class GateDiagnosticQualification(unittest.TestCase):
    def test_missing_receipt_warning_fails_even_if_other_output_is_successful(self):
        for phase in ('record', 'replay'):
            with self.subTest(phase=phase):
                check_gate_diagnostics(phase, 'ordinary warning\n')
                with self.assertRaisesRegex(RuntimeError, phase + ' reported unconfirmed host process cleanup'):
                    check_gate_diagnostics(phase, 'asb process gate cleanup: native gate exited without a quiescence receipt\n')


if __name__ == '__main__':
    unittest.main()
