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


def format_single(run):
    """Render one dnsperf run. Used by the userspace performance job, where
    there is no second mode to compare against; the committed baseline in
    docs/benchmarks/userspace-baseline.md is the comparison instead."""
    if run is None:
        return (
            "## Userspace Performance Report\n\n"
            "The userspace stress run did not produce parseable output.\n"
        )

    return f"""## Userspace Performance Report

| Metric | Value |
|--------|-------|
| Queries per second | {fmt(run['qps'])} |
| Queries sent | {run['sent']} |
| Completed | {run['completed']} |
| Lost | {run['lost']} |
| Average latency (s) | {fmt(run['latency_avg'])} |
| Min latency (s) | {fmt(run['latency_min'])} |
| Max latency (s) | {fmt(run['latency_max'])} |

> Measured with `RUST_LOG=warn` on the `forward` plugin, so per-query `info`
> logs are suppressed and this is not the default configuration's number.
> Compare against docs/benchmarks/userspace-baseline.md; shared-runner variance
> makes the absolute value meaningless on its own.
"""


if __name__ == "__main__":
    args = sys.argv[1:]

    if args and args[0] == "--single":
        if len(args) != 2:
            print("Usage: xdp_stress_report.py --single <run.txt>", file=sys.stderr)
            sys.exit(1)
        print(format_single(parse_dnsperf(args[1])))
    elif len(args) == 2:
        print(format_report(parse_dnsperf(args[0]), parse_dnsperf(args[1])))
    else:
        print(
            "Usage: xdp_stress_report.py <userspace.txt> <xdp.txt> | "
            "xdp_stress_report.py --single <run.txt>",
            file=sys.stderr,
        )
        sys.exit(1)
