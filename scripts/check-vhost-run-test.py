#!/usr/bin/env python3
"""Test evidence rejection using fabricated logs, not a VM validation result."""
import copy
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("checker", Path(__file__).with_name("check-vhost-run.py"))
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


def logs(zero=True):
    host = [dict(event="features", access_platform=True, ring_reset=True),
            dict(event="session_generated", session=42, packets=10000, payload_staging_copies=0),
            dict(event="rx_sample", session=42, sequence=0, frame_gpa="0x40000100")]
    guest = [dict(event="guest_ready", session=42, attach="Driver", zero_copy=zero, address_check=True),
             dict(event="guest_sample", session=42, sequence=0, backend_frame_gpa="0x40000100",
                  umem_frame_gpa="0x40000100" if zero else "0x40001100", same_frame=zero),
             dict(event="guest_result", session=42, ok=True, received=10000, requested=10000,
                  zero_copy=zero, address_check=True, samples=1, physical_matches=int(zero),
                  pending_tx=0, rx_invalid=0, rx_dropped=0)]
    return host, guest


class Evidence(unittest.TestCase):
    def test_positive_and_negative_copy_controls(self):
        for zero in (False, True):
            self.assertEqual(checker.check(*logs(zero), "zero-copy" if zero else "copy"), (42, 1, 10000))

    def test_incomplete_or_contradictory_evidence_rejected(self):
        for section, row, field, value in [
            (0, 1, "session", 43), (0, 1, "payload_staging_copies", 1),
            (0, 2, "frame_gpa", "0x50000100"), (0, 0, "ring_reset", False),
            (1, 0, "attach", "Generic"), (1, 1, "umem_frame_gpa", "0x60000100"),
            (1, 1, "sequence", 1), (1, 2, "pending_tx", 1),
            (1, 2, "address_check", False), (1, 2, "received", 9999),
            (1, 2, "rx_dropped", 1), (1, 2, "samples", 0),
        ]:
            with self.subTest(field=field):
                pair = copy.deepcopy(logs())
                pair[section][row][field] = value
                with self.assertRaises(ValueError):
                    checker.check(*pair, "zero-copy")

    def test_copy_control_cannot_report_same_address(self):
        host, guest = logs(False)
        guest[1]["umem_frame_gpa"] = guest[1]["backend_frame_gpa"]
        with self.assertRaises(ValueError):
            checker.check(host, guest, "copy")


if __name__ == "__main__":
    unittest.main()
