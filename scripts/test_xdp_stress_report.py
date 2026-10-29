import unittest

from xdp_stress_report import format_report, format_single, parse_dnsperf

USERSPACE_SAMPLE = """[18:11:08] dnsperf: awaiting connections
[18:11:08] dnsperf: opening data file
Queries sent: 1298645
Queries completed: 1298645
Queries lost: 0
Queries per second: 43288.123
Average Latency (s): 0.000921 (min 0.000102, max 0.051234)
"""


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
        parsed = parse_dnsperf("scripts/tests/fixtures/userspace.txt")
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


if __name__ == "__main__":
    unittest.main()
