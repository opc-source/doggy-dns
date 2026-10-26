#!/usr/bin/env python3
"""Parse dnsperf text output and generate a markdown comparison report."""

import re
import sys


def parse_dnsperf(path):
    with open(path) as f:
        text = f.read()

    if "Queries per second:" not in text:
        return None

    def extract(pattern, default=0.0):
        m = re.search(pattern, text)
        return float(m.group(1)) if m else default

    def extract_int(pattern, default=0):
        m = re.search(pattern, text)
        return int(m.group(1)) if m else default

    latency_m = re.search(
        r"Average Latency \(s\):\s+([\d.]+)\s+\(min ([\d.]+), max ([\d.]+)\)", text
    )

    return {
        "qps": extract(r"Queries per second:\s+([\d.]+)"),
        "sent": extract_int(r"Queries sent:\s+(\d+)"),
        "completed": extract_int(r"Queries completed:\s+(\d+)"),
        "lost": extract_int(r"Queries lost:\s+(\d+)"),
        "latency_avg": float(latency_m.group(1)) if latency_m else 0,
        "latency_min": float(latency_m.group(2)) if latency_m else 0,
        "latency_max": float(latency_m.group(3)) if latency_m else 0,
    }


def fmt(val, unit=""):
    if val == 0:
        return "N/A"
    if isinstance(val, float) and val > 100:
        return f"{val:,.0f}{unit}"
    if isinstance(val, float):
        return f"{val:.4f}{unit}"
    return f"{val}{unit}"


def format_report(userspace, xdp):
    if userspace is None and xdp is None:
        return "## XDP Stress Test Report\n\nBoth runs failed — no data to compare.\n"

    if userspace is None:
        return "## XDP Stress Test Report\n\nUserspace run failed — no baseline for comparison.\n"

    u_qps = userspace["qps"]
    x_qps = xdp["qps"] if xdp else 0
    ratio = x_qps / u_qps if u_qps > 0 else 0

    report = f"""## XDP Stress Test Report

### Throughput

| Mode | QPS | Queries Sent | Completed | Lost |
|------|-----|-------------|-----------|------|
| Userspace-only | {fmt(u_qps)} | {userspace['sent']} | {userspace['completed']} | {userspace['lost']} |
| XDP | {fmt(x_qps)} | {xdp['sent'] if xdp else 'N/A'} | {xdp['completed'] if xdp else 'N/A'} | {xdp['lost'] if xdp else 'N/A'} |
| **Ratio** | **{ratio:.2f}x** | | | |

### Latency (seconds)

| Mode | Average | Min | Max |
|------|---------|-----|-----|
| Userspace-only | {fmt(userspace['latency_avg'])} | {fmt(userspace['latency_min'])} | {fmt(userspace['latency_max'])} |
| XDP | {fmt(xdp['latency_avg']) if xdp else 'N/A'} | {fmt(xdp['latency_min']) if xdp else 'N/A'} | {fmt(xdp['latency_max']) if xdp else 'N/A'} |

> Note: XDP stress test runs with `continue-on-error: true` because GitHub Actions
> runners may not support XDP attachment to loopback. If XDP data shows N/A or
> ratio is ~1.0x, the XDP fast path was not active.
"""
    return report


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(
            "Usage: xdp_stress_report.py <userspace.txt> <xdp.txt>",
            file=sys.stderr,
        )
        sys.exit(1)

    userspace = parse_dnsperf(sys.argv[1])
    xdp = parse_dnsperf(sys.argv[2])
    print(format_report(userspace, xdp))
