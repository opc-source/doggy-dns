#!/usr/bin/env python3
"""Local userspace-vs-XDP A/B benchmark for a CI-built doggy-dns bundle.

The bundle is produced by `.github/workflows/perf-bundle.yml` and fetched with
`scripts/fetch_perf_bundle.sh`, so the machine running the benchmark never
compiles the workspace.

    sudo python3 scripts/perf_ab.py --bundle dist/perf-bundle-<run-id> --rounds 2

Each round warms both arms against one local upstream: the userspace arm uses
the binary built without the `xdp` feature and answers from hickory's resolver
cache; the XDP arm uses the binary built with it and answers the same names in
the kernel. The local upstream's SIGUSR1 query counter verifies that both caches
were warm, and the XDP_STATS counters plus the server process's CPU time verify
that the kernel really answered the measured traffic rather than falling back
to userspace.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

PIN_DIR = Path("/sys/fs/bpf/doggy-dns-perf")
PIN_FILE = PIN_DIR / "dns_cache_v1"
BPFFS = Path("/sys/fs/bpf")


class BenchError(RuntimeError):
    pass


def log(message: str) -> None:
    print(message, flush=True)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise BenchError(message)


def tail(path: Path, lines: int = 40) -> str:
    try:
        return "\n".join(path.read_text(errors="replace").splitlines()[-lines:])
    except OSError:
        return f"<could not read {path}>"


def port_free(port: int, tcp: bool = True) -> bool:
    sock_types = (socket.SOCK_DGRAM, socket.SOCK_STREAM) if tcp else (socket.SOCK_DGRAM,)
    for sock_type in sock_types:
        sock = socket.socket(socket.AF_INET, sock_type)
        try:
            sock.bind(("127.0.0.1", port))
        except OSError:
            return False
        finally:
            sock.close()
    return True


def dns_probe(port: int, name: str, timeout: float = 0.4) -> bool:
    ident = os.urandom(2)
    question = bytearray(ident)
    question += b"\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00"
    for label in name.rstrip(".").split("."):
        encoded = label.encode()
        question += bytes((len(encoded),)) + encoded
    question += b"\x00\x00\x01\x00\x01"

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(timeout)
    try:
        sock.sendto(question, ("127.0.0.1", port))
        data, _ = sock.recvfrom(4096)
        return len(data) >= 12 and data[:2] == ident and (data[2] & 0x80) != 0
    except OSError:
        return False
    finally:
        sock.close()


def parse_dnsperf(path: Path) -> dict | None:
    text = path.read_text(errors="replace")
    if "Queries per second:" not in text:
        return None

    def number(pattern: str) -> float:
        match = re.search(pattern, text)
        return float(match.group(1)) if match else 0.0

    def integer(pattern: str) -> int:
        match = re.search(pattern, text)
        return int(match.group(1)) if match else 0

    latency = re.search(
        r"Average Latency \(s\):\s+([\d.]+)\s+\(min ([\d.]+), max ([\d.]+)\)",
        text,
    )
    return {
        "qps": number(r"Queries per second:\s+([\d.]+)"),
        "sent": integer(r"Queries sent:\s+(\d+)"),
        "completed": integer(r"Queries completed:\s+(\d+)"),
        "lost": integer(r"Queries lost:\s+(\d+)"),
        "latency_avg": float(latency.group(1)) if latency else 0.0,
        "latency_min": float(latency.group(2)) if latency else 0.0,
        "latency_max": float(latency.group(3)) if latency else 0.0,
    }


def dump_upstream_count(process: subprocess.Popen, count_file: Path) -> int:
    before = count_file.stat().st_size if count_file.exists() else 0
    process.send_signal(signal.SIGUSR1)
    deadline = time.monotonic() + 2.0
    while time.monotonic() < deadline:
        try:
            if count_file.stat().st_size > before:
                break
        except OSError:
            pass
        time.sleep(0.01)
    lines = count_file.read_text().splitlines()
    require(bool(lines), f"upstream count file {count_file} is empty")
    return int(lines[-1])


def xdp_attachment(interface: str) -> tuple[bool, str]:
    result = subprocess.run(
        ["ip", "-d", "link", "show", "dev", interface],
        capture_output=True,
        text=True,
        check=False,
    )
    output = result.stdout
    return bool(re.search(r"\bxdp\w*", output, re.IGNORECASE)), output


def process_cpu_seconds(pid: int) -> float:
    stat = Path(f"/proc/{pid}/stat").read_text()
    fields = stat[stat.rfind(")") + 2 :].split()
    ticks = int(fields[11]) + int(fields[12])  # utime, stime
    return ticks / os.sysconf("SC_CLK_TCK")


def _stats_bytes(value) -> bytes:
    if isinstance(value, str):
        return bytes.fromhex(value.removeprefix("0x"))
    if isinstance(value, list):
        if value and isinstance(value[0], str):
            return bytes(int(item, 16) for item in value)
        return bytes(value)
    raise BenchError(f"unexpected XDP_STATS value encoding: {type(value).__name__}")


def xdp_stats_snapshot(raw_path: Path) -> dict | None:
    """Sum the per-CPU XDP_STATS counters, or None if they cannot be read."""
    result = subprocess.run(
        ["bpftool", "-j", "map", "dump", "name", "XDP_STATS"],
        capture_output=True,
        text=True,
        check=False,
    )
    raw_path.write_text(result.stdout + result.stderr)
    if result.returncode != 0:
        return None
    try:
        entries = json.loads(result.stdout)
        totals = {"hit": 0, "negative_hit": 0, "miss": 0, "expired": 0, "drop": 0}
        for entry in entries:
            per_cpu = entry.get("values")
            if per_cpu is None:
                per_cpu = [entry]
            for cpu_entry in per_cpu:
                value = cpu_entry.get("value")
                if isinstance(value, dict):
                    totals["hit"] += int(value.get("hit", 0))
                    totals["negative_hit"] += int(value.get("negative_hit", 0))
                    totals["miss"] += int(value.get("miss", 0))
                    totals["expired"] += int(value.get("expired", 0))
                    totals["drop"] += int(value.get("drop", 0))
                else:
                    raw = _stats_bytes(value)
                    for offset in range(0, len(raw), 40):
                        chunk = raw[offset : offset + 40]
                        if len(chunk) != 40:
                            break
                        hit, negative, miss, expired, drop = struct.unpack("<5Q", chunk)
                        totals["hit"] += hit
                        totals["negative_hit"] += negative
                        totals["miss"] += miss
                        totals["expired"] += expired
                        totals["drop"] += drop
        return totals
    except (ValueError, TypeError, KeyError, BenchError):
        return None


def clear_pinned_cache() -> None:
    # The userspace-only smoke runs without root, and /sys/fs/bpf is root-only:
    # there, even probing for our file can raise EACCES instead of ENOENT.
    try:
        PIN_FILE.unlink()
    except (FileNotFoundError, PermissionError):
        pass
    try:
        PIN_DIR.rmdir()
    except OSError:
        pass


def wait_detached(interface: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        attached, _ = xdp_attachment(interface)
        if not attached:
            return
        time.sleep(0.1)
    raise BenchError(f"an XDP program is still attached to {interface}")


def write_config(
    path: Path,
    port: int,
    upstream_port: int,
    interface: str,
    worker_threads: int,
    xdp: bool,
) -> None:
    pin_path = PIN_DIR / "dns_cache"
    enabled = "true" if xdp else "false"
    path.write_text(
        f"""[server]
listen_addr = "127.0.0.1"
port = {port}
worker_threads = {worker_threads}

[xdp]
enabled = {enabled}
interface = "{interface}"
cache_size = 65536
negative_ttl = 300
attach_mode = "auto"
pin_path = "{pin_path}"

[[plugins]]
kind = "forward"
enabled = true
upstream = ["127.0.0.1:{upstream_port}"]
"""
    )


def write_queries(path: Path, count: int) -> None:
    path.write_text(
        "".join(f"svc-{index}.perf.doggy.test A\n" for index in range(1, count + 1))
    )


def start_process(
    command: list[str],
    log_path: Path,
    env: dict[str, str] | None = None,
) -> tuple[subprocess.Popen, object]:
    log_file = log_path.open("wb")
    process = subprocess.Popen(
        command,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        env=env,
        start_new_session=True,
    )
    return process, log_file


def stop_process(process: subprocess.Popen, log_file, name: str) -> None:
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            log(f"{name} ignored SIGTERM; killing it")
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)
    log_file.close()


def wait_ready(
    process: subprocess.Popen, port: int, log_path: Path, timeout: float = 20.0
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        require(
            process.poll() is None,
            f"server exited with {process.returncode} before becoming ready:\n{tail(log_path)}",
        )
        if dns_probe(port, "ready.perf.doggy.test"):
            return
        time.sleep(0.2)
    raise BenchError(f"server did not answer within {timeout:.0f}s:\n{tail(log_path)}")


def run_dnsperf(
    binary: str,
    port: int,
    queries: Path,
    threads: int,
    cycles: int,
    duration: int,
    output: Path,
) -> subprocess.CompletedProcess:
    result = subprocess.run(
        [
            binary,
            "-s",
            "127.0.0.1",
            "-p",
            str(port),
            "-d",
            str(queries),
            "-n",
            str(cycles),
            "-T",
            str(threads),
            "-l",
            str(duration),
        ],
        capture_output=True,
        text=True,
        timeout=duration + 30,
        check=False,
    )
    output.write_text(result.stdout + result.stderr)
    return result


def run_arm(
    *,
    arm: str,
    round_number: int,
    args: argparse.Namespace,
    bundle: Path,
    out_dir: Path,
    queries: Path,
    upstream: subprocess.Popen,
    upstream_count: Path,
) -> dict:
    binary = bundle / "bin" / ("doggy-dns-xdp" if arm == "xdp" else "doggy-dns")
    require(binary.is_file(), f"missing {binary} in the bundle")
    config = out_dir / f"{arm}.toml"
    write_config(
        config,
        args.port,
        args.upstream_port,
        args.interface,
        args.worker_threads,
        xdp=(arm == "xdp"),
    )

    tag = f"round{round_number}-{arm}"
    server_log = out_dir / f"{tag}.server.log"
    warmup_output = out_dir / f"{tag}.warmup.txt"
    measured_output = out_dir / f"{tag}.dnsperf.txt"

    if arm == "xdp":
        wait_detached(args.interface)
        clear_pinned_cache()

    env = os.environ.copy()
    env["RUST_LOG"] = "warn"
    server, server_log_file = start_process([str(binary), str(config)], server_log, env)

    row: dict = {
        "round": round_number,
        "arm": arm,
        "ok": False,
        "error": "",
        "server_log": str(server_log),
        "raw": str(measured_output),
        "xdp_attached": False,
    }
    try:
        wait_ready(server, args.port, server_log)

        warmup = run_dnsperf(
            args.dnsperf,
            args.port,
            queries,
            args.threads,
            2,
            min(10, args.duration),
            warmup_output,
        )
        if warmup.returncode != 0:
            log(f"{tag}: warm-up exited {warmup.returncode}; see {warmup_output}")
        time.sleep(1.0)

        cpu_before = process_cpu_seconds(server.pid)
        stats_before = None
        if arm == "xdp":
            stats_before = xdp_stats_snapshot(out_dir / f"{tag}.stats-before.json")
        count_before = dump_upstream_count(upstream, upstream_count)

        measured = run_dnsperf(
            args.dnsperf,
            args.port,
            queries,
            args.threads,
            100000,
            args.duration,
            measured_output,
        )
        time.sleep(0.5)

        cpu_delta = process_cpu_seconds(server.pid) - cpu_before
        stats_after = None
        if arm == "xdp":
            stats_after = xdp_stats_snapshot(out_dir / f"{tag}.stats-after.json")
        count_after = dump_upstream_count(upstream, upstream_count)

        parsed = parse_dnsperf(measured_output)
        require(parsed is not None, f"dnsperf produced no parseable output:\n{tail(measured_output)}")
        require(
            measured.returncode == 0,
            f"dnsperf exited {measured.returncode}:\n{tail(measured_output)}",
        )

        attached, attachment_detail = xdp_attachment(args.interface)
        (out_dir / f"{tag}.ip-link.txt").write_text(attachment_detail)

        upstream_delta = count_after - count_before
        completed = parsed["completed"]
        require(completed > 0, "dnsperf completed no queries")
        row.update(parsed)
        row["upstream_queries"] = upstream_delta
        row["xdp_attached"] = attached
        row["cpu_seconds"] = cpu_delta
        row["xdp_hits"] = None
        row["xdp_misses"] = None

        # Both arms warm their cache during the warm-up pass: the userspace
        # hickory resolver has its own cache. Upstream staying idle is what
        # makes this a cache-hit comparison rather than a miss-path number.
        allowed_refreshes = max(20, int(completed * 0.01))
        require(
            upstream_delta <= allowed_refreshes,
            f"{arm} round forwarded {upstream_delta} of {completed} queries upstream "
            f"(allowed {allowed_refreshes}); the cache was not warm, so this run would "
            "measure the upstream path instead of the cache-hit path",
        )

        if arm == "xdp":
            require(
                attached,
                f"XDP was not attached to {args.interface} after the measured run",
            )
            if stats_before is not None and stats_after is not None:
                hits = (stats_after["hit"] + stats_after["negative_hit"]) - (
                    stats_before["hit"] + stats_before["negative_hit"]
                )
                misses = stats_after["miss"] - stats_before["miss"]
                row["xdp_hits"] = hits
                row["xdp_misses"] = misses
                require(
                    hits >= completed * 0.5,
                    f"XDP counters moved by only {hits} hits for {completed} completed "
                    f"queries (misses {misses}); the kernel fast path did not answer",
                )
            else:
                log(f"{tag}: XDP_STATS not readable; relying on server CPU time")
            require(
                cpu_delta < max(0.5, args.duration * 0.5),
                f"the userspace process burned {cpu_delta:.2f} CPU-seconds during the "
                "measured window; XDP hits should have bypassed it",
            )

        row["ok"] = True
        stats_note = ""
        if row["xdp_hits"] is not None:
            stats_note = f", XDP hits {row['xdp_hits']:,}, misses {row['xdp_misses']:,}"
        log(
            f"{tag}: {parsed['qps']:,.0f} QPS, avg {parsed['latency_avg'] * 1000:.3f} ms, "
            f"lost {parsed['lost']}, upstream {upstream_delta}, "
            f"server CPU {cpu_delta:.2f}s{stats_note}"
        )
    except Exception as error:  # one bad arm must not lose the other arm's data
        row["error"] = str(error) or error.__class__.__name__
        log(f"{tag}: FAILED: {row['error']}")
    finally:
        stop_process(server, server_log_file, f"{tag} server")
        if arm == "xdp":
            clear_pinned_cache()

    return row


def median_of(rows: list[dict], key: str) -> float | None:
    values = [row[key] for row in rows if row["ok"] and row.get(key) is not None]
    return statistics.median(values) if values else None


def fmt(value: float | None, digits: int = 0, suffix: str = "") -> str:
    if value is None:
        return "N/A"
    return f"{value:,.{digits}f}{suffix}"


def render_report(
    args: argparse.Namespace,
    bundle: Path,
    rows: list[dict],
    medians: dict,
    ratio: float | None,
    host: str,
    bundle_manifest: str,
) -> str:
    lines = [
        "# doggy-dns local A/B: userspace vs XDP",
        "",
        f"- Date: {datetime.now(timezone.utc).isoformat(timespec='seconds')}",
        f"- Host: `{host}`",
        f"- Bundle: `{bundle}` (CI-built; no local Rust compilation)",
        f"- Load: `dnsperf -n 100000 -T {args.threads} -l {args.duration}` "
        f"over {args.query_count} names, `RUST_LOG=warn`",
        f"- Upstream: local `perf-upstream` on `127.0.0.1:{args.upstream_port}` "
        "(fixed A record, TTL 300)",
        f"- XDP interface: `{args.interface}`; loopback attaches in generic mode",
        f"- Rounds: {args.rounds} (arm order alternates between rounds)",
        "",
        "## Build manifest",
        "",
        "```",
        bundle_manifest.strip(),
        "```",
        "",
        "## Per-run",
        "",
        "| Round | Arm | QPS | Avg latency (ms) | Lost | Server CPU (s) | Upstream queries | XDP attached | XDP hits |",
        "|------:|-----|----:|-----------------:|-----:|---------------:|-----------------:|:------------:|---------:|",
    ]

    for row in rows:
        if row["ok"]:
            hits = "—" if row.get("xdp_hits") is None else f"{row['xdp_hits']:,}"
            lines.append(
                f"| {row['round']} | {row['arm']} | {row['qps']:,.0f} | "
                f"{row['latency_avg'] * 1000:.3f} | {row['lost']} | "
                f"{row['cpu_seconds']:.2f} | {row['upstream_queries']} | "
                f"{'yes' if row['xdp_attached'] else 'no'} | {hits} |"
            )
        else:
            lines.append(
                f"| {row['round']} | {row['arm']} | FAIL | — | — | — | — | "
                f"{'yes' if row['xdp_attached'] else 'no'} | — |"
            )

    failed = [row for row in rows if not row["ok"]]
    if failed:
        lines += ["", "## Failures", ""]
        for row in failed:
            lines.append(f"- round {row['round']} {row['arm']}: {row['error']}")

    lines += [
        "",
        "## Medians",
        "",
        "| Arm | QPS | Avg latency (ms) | Lost | Server CPU (s) | Upstream queries |",
        "|-----|----:|-----------------:|-----:|---------------:|-----------------:|",
    ]
    for arm in ("userspace", "xdp"):
        lines.append(
            f"| {arm} | {fmt(medians[arm]['qps'])} | "
            f"{fmt(None if medians[arm]['latency_avg'] is None else medians[arm]['latency_avg'] * 1000, 3)} | "
            f"{fmt(medians[arm]['lost'])} | {fmt(medians[arm]['cpu_seconds'], 2)} | "
            f"{fmt(medians[arm]['upstream_queries'])} |"
        )
    if ratio is not None:
        lines += ["", f"**XDP / userspace throughput: {ratio:.2f}x**"]
        if medians["xdp"]["cpu_seconds"] and medians["userspace"]["cpu_seconds"]:
            cpu_ratio = medians["userspace"]["cpu_seconds"] / medians["xdp"]["cpu_seconds"]
            lines.append(
                f"**Userspace / XDP server CPU time: {cpu_ratio:.2f}x** "
                "(how much less CPU the server burned when the kernel answered)"
            )
    else:
        lines += ["", "**Ratio unavailable: one arm has no successful run.**"]

    lines += [
        "",
        "## How to read this",
        "",
        "- Both arms are **warm-cache**. The userspace arm (binary without the `xdp` "
        "feature) resolves through hickory's own resolver cache; the XDP arm answers the "
        "same warmed names from the kernel cache before the packet reaches userspace. "
        "This is the fast-path comparison, not a cold-cache or miss-path number.",
        "- `Upstream queries` is the local upstream's counter delta across the measured "
        "window. It must stay near zero on both arms: nonzero values close to the "
        "completed count mean the warm-up did not populate the cache and the run measured "
        "the upstream path instead.",
        "- `XDP hits` is the delta of the eBPF program's per-CPU `hit` + `negative_hit` "
        "counters. `Server CPU` is the userspace process's CPU time across the measured "
        "window. Together they show the kernel answered: hits close to the completed "
        "count, and a server process that was nearly idle.",
        "- Both arms share this host with `dnsperf` and the upstream process. Absolute QPS "
        "is therefore a loopback/generic-XDP number, not NIC-native XDP throughput.",
        "",
        f"Raw dnsperf output, server logs, XDP stats and `ip -d link show` captures: `{args.out}`",
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True, help="fetched bundle directory")
    parser.add_argument("--rounds", type=int, default=2, help="rounds per arm (default 2)")
    parser.add_argument("--duration", type=int, default=30, help="measured seconds per run")
    parser.add_argument("--threads", type=int, default=max(2, os.cpu_count() or 2))
    parser.add_argument("--worker-threads", type=int, default=max(2, os.cpu_count() or 2))
    parser.add_argument("--queries", type=int, default=1000, help="distinct names in the query set")
    parser.add_argument("--port", type=int, default=15353)
    parser.add_argument("--upstream-port", type=int, default=15354)
    parser.add_argument("--interface", default="lo")
    parser.add_argument("--dnsperf", default="dnsperf")
    parser.add_argument(
        "--only",
        choices=("both", "userspace", "xdp"),
        default="both",
        help="run only one arm (useful for smoke tests)",
    )
    parser.add_argument("--out", type=Path, help="output directory (default: perf-results/<timestamp>)")
    parser.add_argument(
        "--emit-configs",
        type=Path,
        metavar="DIR",
        help="write the reference userspace.toml/xdp.toml for these settings to DIR and exit",
    )
    args = parser.parse_args()

    args.rounds = max(1, args.rounds)
    args.duration = max(2, args.duration)
    args.threads = max(1, args.threads)
    args.worker_threads = max(1, args.worker_threads)
    args.query_count = max(10, args.queries)

    if args.emit_configs is not None:
        args.emit_configs.mkdir(parents=True, exist_ok=True)
        for arm in ("userspace", "xdp"):
            write_config(
                args.emit_configs / f"{arm}.toml",
                args.port,
                args.upstream_port,
                args.interface,
                args.worker_threads,
                xdp=arm == "xdp",
            )
        print(f"configs written to {args.emit_configs}")
        return 0

    if args.out is None:
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        args.out = Path("perf-results") / stamp
    args.out.mkdir(parents=True, exist_ok=True)

    bundle = args.bundle.resolve()
    require(bundle.is_dir(), f"bundle directory not found: {bundle}")
    require(shutil.which(args.dnsperf) is not None, f"{args.dnsperf} is not on PATH")
    require(shutil.which("ip") is not None, "iproute2 (ip) is required")
    for name in ("doggy-dns", "doggy-dns-xdp", "perf-upstream"):
        require((bundle / "bin" / name).is_file(), f"missing bin/{name} in {bundle}")

    if args.only in ("both", "xdp"):
        require(os.geteuid() == 0, "XDP arms need root; re-run with sudo")
        require(shutil.which("bpftool") is not None, "bpftool is required for the XDP counters")
        with open("/proc/mounts") as mounts:
            require(
                any(
                    len(parts) >= 3 and parts[1] == str(BPFFS) and parts[2] == "bpf"
                    for parts in (line.split() for line in mounts)
                ),
                f"bpffs is not mounted at {BPFFS}; mount it with "
                f"`sudo mount -t bpf bpffs {BPFFS}`",
            )

    require(port_free(args.port), f"port {args.port}/udp+tcp is already in use")
    require(
        port_free(args.upstream_port, tcp=False),
        f"upstream port {args.upstream_port}/udp is already in use",
    )

    queries = args.out / "queries.txt"
    write_queries(queries, args.query_count)

    upstream_count = args.out / "upstream.count"
    upstream_log = args.out / "upstream.log"
    upstream, upstream_log_file = start_process(
        [
            str(bundle / "bin" / "perf-upstream"),
            "--port",
            str(args.upstream_port),
            "--ip",
            "127.0.0.9",
            "--ttl",
            "300",
            "--count-file",
            str(upstream_count),
        ],
        upstream_log,
    )

    rows: list[dict] = []
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not dns_probe(args.upstream_port, "probe.perf.doggy.test"):
            require(
                upstream.poll() is None,
                f"perf-upstream exited early:\n{tail(upstream_log)}",
            )
            time.sleep(0.1)
        require(
            dns_probe(args.upstream_port, "probe.perf.doggy.test"),
            f"perf-upstream is not answering:\n{tail(upstream_log)}",
        )

        for round_number in range(1, args.rounds + 1):
            order = ("userspace", "xdp") if round_number % 2 == 1 else ("xdp", "userspace")
            for arm in order:
                if args.only != "both" and arm != args.only:
                    continue
                log(f"round {round_number}: {arm} arm")
                rows.append(
                    run_arm(
                        arm=arm,
                        round_number=round_number,
                        args=args,
                        bundle=bundle,
                        out_dir=args.out,
                        queries=queries,
                        upstream=upstream,
                        upstream_count=upstream_count,
                    )
                )
    finally:
        stop_process(upstream, upstream_log_file, "perf-upstream")
        if args.only in ("both", "xdp"):
            clear_pinned_cache()

    medians = {
        arm: {
            key: median_of([row for row in rows if row["arm"] == arm], key)
            for key in (
                "qps",
                "latency_avg",
                "lost",
                "upstream_queries",
                "cpu_seconds",
                "xdp_hits",
            )
        }
        for arm in ("userspace", "xdp")
    }
    ratio = None
    if (
        medians["userspace"]["qps"]
        and medians["xdp"]["qps"]
        and any(row["arm"] == "userspace" and row["ok"] for row in rows)
        and any(row["arm"] == "xdp" and row["ok"] for row in rows)
    ):
        ratio = medians["xdp"]["qps"] / medians["userspace"]["qps"]

    manifest = ""
    manifest_path = bundle / "MANIFEST.txt"
    if manifest_path.is_file():
        manifest = manifest_path.read_text()
    else:
        manifest = "MANIFEST.txt not present in bundle"

    report = render_report(
        args,
        bundle,
        rows,
        medians,
        ratio,
        f"{os.uname().sysname} {os.uname().release} {os.uname().machine}",
        manifest,
    )
    report_path = args.out / "report.md"
    report_path.write_text(report)
    (args.out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")

    print()
    print(report)
    log(f"report written to {report_path}")

    expected_arms = ["userspace", "xdp"] if args.only == "both" else [args.only]
    complete = all(
        any(row["arm"] == arm and row["ok"] for row in rows) for arm in expected_arms
    )
    return 0 if complete else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BenchError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
