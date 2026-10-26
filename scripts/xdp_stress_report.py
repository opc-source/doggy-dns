#!/usr/bin/env python3
"""Parse dnsperf JSON output and generate a markdown comparison report."""

import json
import sys


def load_results(path):
    with open(path) as f:
        return json.load(f)


def format_report(userspace, xdp):
    u_qps = userspace.get("queries_per_second", 0)
    x_qps = xdp.get("queries_per_second", 0)
    ratio = x_qps / u_qps if u_qps > 0 else 0

    u_lat = userspace.get("latency", {})
    x_lat = xdp.get("latency", {})

    report = f"""## XDP Stress Test Report

### Throughput

| Mode | QPS |
|------|-----|
| Userspace-only | {u_qps:,.0f} |
| XDP (cache hit) | {x_qps:,.0f} |
| **Improvement** | **{ratio:.1f}x** |

### Latency

| Mode | Average |
|------|---------|
| Userspace-only | {u_lat.get("average", "N/A")} |
| XDP (cache hit) | {x_lat.get("average", "N/A")} |
"""
    return report


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(
            "Usage: xdp_stress_report.py <userspace.json> <xdp.json>",
            file=sys.stderr,
        )
        sys.exit(1)

    userspace = load_results(sys.argv[1])
    xdp = load_results(sys.argv[2])
    print(format_report(userspace, xdp))
