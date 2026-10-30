import unittest
from pathlib import Path

from xdp_stress_report import format_report, format_single, parse_dnsperf

# Resolved from this file, not the cwd: the suite has to pass from the repo
# root (python3 scripts/test_xdp_stress_report.py) and from scripts/ alike.
FIXTURE = Path(__file__).parent / "tests" / "fixtures" / "userspace.txt"


class SingleMode(unittest.TestCase):
    def setUp(self):
        self.parsed = {
            "qps": 43288.123,
            "sent": 1298645,
            "completed": 1298645,
            "lost": 0,
            "latency_avg": 0.000921,
            "latency_min": 0.000102,
            "latency_max": 0.051234,
        }

    def test_parse_reads_qps_and_latency(self):
        parsed = parse_dnsperf(FIXTURE)
        self.assertEqual(parsed["qps"], 43288.123)
        self.assertEqual(parsed["lost"], 0)

    def test_single_mode_renders_one_run(self):
        report = format_single(self.parsed)
        self.assertIn("## Userspace Performance Report", report)
        self.assertIn("43,288", report)
        self.assertIn("0.0009", report)

    def test_single_mode_handles_a_failed_run(self):
        report = format_single(None)
        self.assertIn("did not produce parseable output", report)

    def test_pair_mode_is_unchanged(self):
        report = format_report(self.parsed, None)
        self.assertIn("## XDP Stress Test Report", report)
        self.assertIn("43,288", report)
        self.assertIn("| **Ratio** | **0.00x** |", report)
        self.assertIn("N/A", report)


if __name__ == "__main__":
    unittest.main()
