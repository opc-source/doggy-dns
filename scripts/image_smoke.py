#!/usr/bin/env python3
"""Smoke test a built doggy-dns container image.

Run it on the machine that hosts the container runtime:

    python3 scripts/image_smoke.py --image ghcr.io/org/doggy-dns:main
    python3 scripts/image_smoke.py --image ghcr.io/org/doggy-dns:main-xdp --xdp
    # macOS + podman: copy this file into the VM and run it there
    podman machine ssh … python3 /tmp/image_smoke.py --runtime podman --xdp

What it proves, in order:

1. the image declares the user it is meant to run as (uid 65532 userspace, root
   with the XDP fast path) and, started with no arguments, looks for its config at
   /app/config/doggy-dns.toml — the path the image ships its default at — and the
   config mounted there passes `validate_config`;
2. the control plane answers `/healthz` and then `/readyz` with 200;
3. a raw UDP A query is answered, and the same query over TCP is answered, with
   the address the fake upstream handed out;
4. with `--xdp`, and only then: the pinned `DNS_CACHE` map is created on bpffs,
   survives the container being replaced, and still answers a UDP query after the
   forward upstream has been closed — so the reply came from the reused kernel map
   and not from the network.

Everything is self-contained: the forward upstream is a one-record resolver this
script serves on loopback, so a run needs no outbound internet and no public
resolver, and the expected answer is a fixed TEST-NET address. That is also why
the container runs with `--network host` — it has to be able to reach the script's
port — and why the script pre-checks that the ports it wants are free.

`--xdp` additionally needs bpffs mounted at /sys/fs/bpf on the runtime host and an
interface that accepts an XDP program.
"""

import argparse
import os
import random
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

HOST = "127.0.0.1"

RCODE_NAMES = {
    0: "NOERROR",
    1: "FORMERR",
    2: "SERVFAIL",
    3: "NXDOMAIN",
    4: "NOTIMP",
    5: "REFUSED",
}

QTYPE_NUMBERS = {"A": 1, "AAAA": 28}

# Chosen so they cannot be mistaken for real infrastructure: 192.0.2.0/24 is
# TEST-NET-1, and the pin gets a directory of its own under bpffs.
FAKE_ANSWER_IP = "192.0.2.1"
DEFAULT_XDP_PIN_PATH = "/sys/fs/bpf/doggy-dns-smoke/dns_cache"
REUSE_LOG_LINE = "reusing the pinned DNS_CACHE map"


class NotDnsResponse(Exception):
    """The reply is too short or too malformed to be a DNS header."""


class SmokeFailure(Exception):
    """A check failed; the message is what the operator needs to see."""


# --------------------------------------------------------------------------- #
# DNS wire helpers (pure, covered by scripts/test_image_smoke.py)
# --------------------------------------------------------------------------- #
def build_query(name, qtype=1, txid=0xBEEF):
    """A one-question DNS query: RD set, no EDNS, so any server can answer."""
    labels = name.rstrip(".").split(".")
    question = b"".join(len(label).to_bytes(1, "big") + label.encode() for label in labels)
    question += b"\x00" + struct.pack("!HH", qtype, 1)  # QCLASS = IN
    header = struct.pack("!HHHHHH", txid, 0x0100, 1, 0, 0, 0)
    return header + question


def read_response(packet, expected_txid):
    """Decode a DNS header plus its echoed question."""
    if len(packet) < 12:
        raise NotDnsResponse(f"reply is {len(packet)} bytes, too short for a header")
    txid, flags, qdcount, ancount, nscount, arcount = struct.unpack("!HHHHHH", packet[:12])
    rcode = flags & 0x000F
    qname, offset = _read_name(packet, 12)
    return {
        "is_response": flags >> 15 == 1,
        "txid": txid,
        "rcode": rcode,
        "rcode_name": RCODE_NAMES.get(rcode, f"RCODE{rcode}"),
        "ancount": ancount,
        "nscount": nscount,
        "arcount": arcount,
        "qdcount": qdcount,
        "qname": qname,
        "question_ends_at": offset + 4 * qdcount,
        "txid_mismatch": txid != expected_txid,
    }


def _read_name(packet, offset):
    """Read a name; a compression pointer ends the scan at the pointer target."""
    labels = []
    while offset < len(packet):
        length = packet[offset]
        if length == 0:
            return ".".join(labels), offset + 1
        if length & 0xC0 == 0xC0:  # pointer: the name is already complete here
            return ".".join(labels), offset + 2
        labels.append(packet[offset + 1 : offset + 1 + length].decode("ascii", "replace"))
        offset += 1 + length
    raise NotDnsResponse("name ran past the end of the message")


def read_answers(packet, offset, ancount):
    """Decode the answer records that follow the question section."""
    answers = []
    for _ in range(ancount):
        name, offset = _read_name(packet, offset)
        rtype, rclass, ttl, rdlength = struct.unpack("!HHIH", packet[offset : offset + 10])
        offset += 10
        rdata = packet[offset : offset + rdlength]
        offset += rdlength
        answer = {"name": name, "rtype": rtype, "rclass": rclass, "ttl": ttl, "rdata": rdata}
        if rtype == 1 and rdlength == 4:
            answer["ip"] = ".".join(str(byte) for byte in rdata)
        answers.append(answer)
    return answers


def fake_response(query, ip, ttl, served_name=None):
    """A response to `query`: one A record, or NXDOMAIN if `served_name` is set and differs.

    Leaving `served_name` unset answers every name with the same record, which is
    all the smoke needs; passing it makes the fixture selective.
    """
    txid = int.from_bytes(query[0:2], "big")
    asked_name, name_end = _read_name(query, 12)
    # Only the question record itself: qtype and qclass follow the name, and
    # whatever comes after that belongs to the sections the response declares
    # empty. hickory's forwarder sends EDNS0, so copying `query[12:]` verbatim
    # would leave its OPT record sitting where the answer section starts, and the
    # server would answer its own upstream with SERVFAIL.
    question = query[12:name_end + 4]
    if served_name is not None and asked_name != served_name:
        header = struct.pack("!HHHHHH", txid, 0x8180 | 3, 1, 0, 0, 0)
        return header + question
    # QR | AA | RD | RA with RCODE 0; the owner name is a pointer to offset 12,
    # where the question name starts.
    header = struct.pack("!HHHHHH", txid, 0x8580, 1, 1, 0, 0)
    rdata = bytes(int(part) for part in ip.split("."))
    answer = b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, ttl, len(rdata)) + rdata
    return header + question + answer


# --------------------------------------------------------------------------- #
# The fake forward upstream
# --------------------------------------------------------------------------- #
class FakeUpstream:
    """A one-record resolver on loopback, on UDP and TCP, so the smoke needs no internet.

    TCP is not decoration: doggy-dns forwards a TCP client query over TCP, so an
    answer the smoke asserts over TCP has to have a fixture behind it.

    `stop()` closes both transports for good, which is how the `--xdp` run proves
    that a reply after a container replacement came from the pinned map: there is
    nobody left to forward to.
    """

    def __init__(self, port, served_name=None, ttl=600):
        self.served_name = served_name
        self.ttl = ttl
        self.requests = 0
        self._stop = threading.Event()
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            self._sock.bind((HOST, port))
        except OSError as error:
            raise SmokeFailure(f"cannot bind the fake upstream to {HOST}:{port}: {error}") from error
        # `port=0` asks the kernel for one, and both transports have to end up on
        # the same number, because the config the smoke writes names a single
        # upstream address.
        self.port = self._sock.getsockname()[1]
        self._sock.settimeout(0.2)
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            self._listener.bind((HOST, self.port))
        except OSError as error:
            self._sock.close()
            raise SmokeFailure(f"cannot bind the fake upstream's TCP socket: {error}") from error
        self._listener.listen(8)
        self._listener.settimeout(0.2)
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._tcp_thread = threading.Thread(target=self._serve_tcp, daemon=True)

    def _serve(self):
        while not self._stop.is_set():
            try:
                packet, peer = self._sock.recvfrom(4096)
            except TimeoutError:
                continue
            except OSError:
                return
            self.requests += 1
            try:
                reply = fake_response(packet, FAKE_ANSWER_IP, self.ttl, self.served_name)
            except (NotDnsResponse, struct.error):
                continue  # a malformed probe is not worth killing the fixture over
            self._sock.sendto(reply, peer)

    def _serve_tcp(self):
        while not self._stop.is_set():
            try:
                connection, _ = self._listener.accept()
            except TimeoutError:
                continue
            except OSError:
                return
            with connection:
                self._answer_tcp(connection)

    def _answer_tcp(self, connection):
        connection.settimeout(2.0)
        prefix = self._read_tcp(connection, 2)
        if prefix is None:
            return
        packet = self._read_tcp(connection, struct.unpack("!H", prefix)[0])
        if packet is None:
            return
        self.requests += 1
        try:
            reply = fake_response(packet, FAKE_ANSWER_IP, self.ttl, self.served_name)
        except (NotDnsResponse, struct.error):
            return
        try:
            connection.sendall(struct.pack("!H", len(reply)) + reply)
        except OSError:
            pass  # the client hung up; nothing to report

    @staticmethod
    def _read_tcp(connection, size):
        """Read exactly `size` bytes, or None if the peer stopped early."""
        data = b""
        while len(data) < size:
            try:
                chunk = connection.recv(size - len(data))
            except OSError:
                return None
            if not chunk:
                return None
            data += chunk
        return data

    def __enter__(self):
        self._thread.start()
        self._tcp_thread.start()
        return self

    def __exit__(self, *exc_info):
        self.stop()

    def stop(self):
        self._stop.set()
        self._thread.join(timeout=2.0)
        self._tcp_thread.join(timeout=2.0)
        self._sock.close()
        self._listener.close()


# --------------------------------------------------------------------------- #
# Probing
# --------------------------------------------------------------------------- #
def udp_query(port, name, qtype="A", timeout=3.0, attempts=3):
    txid = random.randrange(1 << 16)
    packet = build_query(name, QTYPE_NUMBERS[qtype], txid)
    last_error = None
    for _ in range(attempts):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.settimeout(timeout)
            sock.sendto(packet, (HOST, port))
            try:
                data, _ = sock.recvfrom(4096)
            except (TimeoutError, OSError) as error:
                last_error = error
                continue
        response = read_response(data, txid)
        response["answers"] = read_answers(data, response["question_ends_at"], response["ancount"])
        return response
    raise SmokeFailure(f"no UDP reply from {HOST}:{port} for {name} after {attempts} tries: {last_error}")


def tcp_query(port, name, qtype="A", timeout=10.0):
    """TCP DNS is length-prefixed; the query body is the same packet."""
    txid = random.randrange(1 << 16)
    packet = build_query(name, QTYPE_NUMBERS[qtype], txid)
    with socket.create_connection((HOST, port), timeout=timeout) as sock:
        sock.sendall(struct.pack("!H", len(packet)) + packet)
        prefix = sock.recv(2)
        if len(prefix) != 2:
            raise NotDnsResponse("TCP reply had no length prefix")
        remaining = struct.unpack("!H", prefix)[0]
        data = b""
        while len(data) < remaining:
            chunk = sock.recv(remaining - len(data))
            if not chunk:
                raise NotDnsResponse("TCP connection closed mid-reply")
            data += chunk
    response = read_response(data, txid)
    response["answers"] = read_answers(data, response["question_ends_at"], response["ancount"])
    return response


def wait_for_http(url, timeout, logs):
    deadline = time.monotonic() + timeout
    last = "no attempt made"
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2.0) as response:
                if response.status == 200:
                    return
                last = f"HTTP {response.status}"
        except urllib.error.HTTPError as error:
            last = f"HTTP {error.code}"
        except (urllib.error.URLError, OSError) as error:
            last = str(error)
        time.sleep(0.5)
    raise SmokeFailure(f"{url} never returned 200 ({last}); container logs:\n{logs()}")


def ensure_ports_free(ports):
    """Fail before starting a container if one of the ports we need is taken.

    With `--network host` the container binds the runtime host's ports, so a
    collision would otherwise surface as an unexplained readiness timeout. Both
    transports are checked because the fixture and the server listen on both.

    `SO_REUSEADDR` is what makes this a check for a *listener* rather than for
    any socket: without it, a run started right after the previous one is stopped
    fails on the previous container's TIME_WAIT connection, which the next
    container can bind over perfectly well.
    """
    for port in ports:
        for kind, sock in (
            ("UDP", socket.socket(socket.AF_INET, socket.SOCK_DGRAM)),
            ("TCP", socket.socket(socket.AF_INET, socket.SOCK_STREAM)),
        ):
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            with sock:
                try:
                    sock.bind((HOST, port))
                except OSError as error:
                    raise SmokeFailure(f"{kind} port {port} is already in use on {HOST}: {error}") from error


# --------------------------------------------------------------------------- #
# Container driving
# --------------------------------------------------------------------------- #
def runtime_command(args, *rest, check=True):
    command = [args.runtime, *rest]
    result = subprocess.run(command, check=False, text=True, capture_output=True)
    if check and result.returncode != 0:
        raise SmokeFailure(f"`{' '.join(command)}` failed ({result.returncode}): {combined(result)}")
    return result


def combined(result):
    return (result.stdout + result.stderr).strip()


def config_text(args, *, upstream, xdp):
    """The smoke config: unprivileged ports so the nonroot image can bind them."""
    xdp_block = ""
    if xdp:
        interface_line = f'interface = "{args.interface}"\n' if args.interface else ""
        xdp_block = f"""
[xdp]
enabled = true
{interface_line}attach_mode = "auto"
pin_path = "{DEFAULT_XDP_PIN_PATH}"
"""
    return f"""[server]
listen_addr = "0.0.0.0"
port = {args.dns_port}
worker_threads = 1

[middleware]
logging = true

[observability]
enabled = false

[[plugins]]
kind = "forward"
enabled = true
upstream = ["{upstream}"]
cache_size = 1024
min_ttl = 60
max_ttl = 600

[control]
enabled = true
listen_addr = "0.0.0.0"
port = {args.control_port}
endpoints = ["healthz", "readyz"]
{xdp_block}"""


def mount_spec(directory, target, runtime):
    """One read-only bind mount, labelled the way each runtime wants it.

    Two details make the difference between "works" and "permission denied":

    * the config directory lives in a temp dir that the *host* user owns, and the
      container process runs as uid 65532, so every parent has to be traversable;
      `TemporaryDirectory` creates 0700, which is not.
    * an SELinux host (the podman machine image) refuses to let an unprivileged
      container read an unlabelled file, hence the `z` option there. Mount options
      are comma-separated — `:ro:z` is a parse error, not two options — and Docker
      on a non-SELinux runner does not take `z` at all, so it stays podman-only.
    """
    os.chmod(directory, 0o755)
    os.chmod(os.path.join(directory, "doggy-dns.toml"), 0o644)
    suffix = ",z" if runtime == "podman" else ""
    return f"{directory}:{target}:ro{suffix}"


def start_container(args, root, *, run_tag, upstream, xdp):
    """Start one instance and return its container name.

    Each run gets its own config directory under `root`: the bind mount is
    read-only, so a replacement container has to be handed a freshly written file
    rather than the one the previous run read.
    """
    directory = os.path.join(root, run_tag)
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, "doggy-dns.toml"), "w") as handle:
        handle.write(config_text(args, upstream=upstream, xdp=xdp))
    name = f"{args.name}-{run_tag}"
    command = ["run", "-d", "--name", name, "--network", "host"]
    if xdp:
        # Loading and attaching an eBPF program needs BPF (or SYS_ADMIN), plus
        # NET_ADMIN to set the interface's XDP fd. PERFMON lets aya read map info,
        # which is what the reuse warning depends on. The fast path writes through
        # to the pinned map, so /sys/fs/bpf has to come from the host.
        command += [
            "-v",
            "/sys/fs/bpf:/sys/fs/bpf",
            "--user",
            "0:0",
            "--cap-drop",
            "ALL",
            "--cap-add",
            "BPF",
            "--cap-add",
            "PERFMON",
            "--cap-add",
            "NET_ADMIN",
        ]
        if args.runtime == "podman":
            # bpffs carries the kernel's own labels and cannot be relabelled by a
            # mount option, so an SELinux host would deny the pin write no matter
            # which capabilities the container has. This relaxes mandatory access
            # control only — the capability set above stays exactly as minimal, and
            # Docker on a non-SELinux runner does not accept the option at all.
            command += ["--security-opt", "label=disable"]
    else:
        # No `--user`: uid 65532 has to come from the image itself, so a plain
        # `docker run` of the published tag is unprivileged without anyone
        # remembering to say so. `--cap-drop ALL` then proves the userspace binary
        # needs no capability when it listens on an unprivileged port.
        command += ["--cap-drop", "ALL"]
    command += ["-v", mount_spec(directory, "/app/config", args.runtime), args.image]
    runtime_command(args, *command)
    return name


def stop_container(args, name):
    runtime_command(args, "stop", "-t", "2", name, check=False)
    runtime_command(args, "rm", "-f", name, check=False)


def container_logs(args, name, tail=150):
    result = runtime_command(args, "logs", name, check=False)
    return "\n".join(combined(result).splitlines()[-tail:])


def expected_user(args):
    """The user the image under test has to declare.

    `--xdp` implies the root-running variant, but CI smokes the XDP image through
    its userspace path (no `--xdp`, no bpffs) and still has to assert the right
    user, so an explicit `--expect-user` wins over the variant.
    """
    if args.expect_user:
        return args.expect_user
    return "root" if args.xdp else "65532:65532"


def check_image_user(args, expected):
    """The image has to declare the user it should run as.

    The smoke starts the userspace image without `--user`, so what it exercises is
    the image's own default; this asserts that default rather than trusting it.
    """
    result = runtime_command(args, "inspect", "--format", "{{.Config.User}}", args.image, check=False)
    declared = combined(result).strip()
    if result.returncode != 0 or declared != expected:
        raise SmokeFailure(
            f"{args.image} declares user {declared!r}, expected {expected!r}: "
            f"{combined(result)}"
        )


def check_pinned_map(args, name):
    """The pinned map file must exist inside the container at its pin path."""
    pinned = f"{DEFAULT_XDP_PIN_PATH}_v1"
    result = runtime_command(args, "exec", name, "test", "-e", pinned, check=False)
    if result.returncode != 0:
        listing = runtime_command(args, "exec", name, "ls", "-laR", "/sys/fs/bpf", check=False)
        raise SmokeFailure(f"no pinned map at {pinned}; /sys/fs/bpf: {combined(listing)}")


# --------------------------------------------------------------------------- #
# Assertions
# --------------------------------------------------------------------------- #
def require_answer(response, name, ip, channel, logs):
    """The response must be a NOERROR answer for `name` carrying exactly `ip`."""
    problems = []
    if not response["is_response"]:
        problems.append("QR is 0, so this is not a response")
    if response["txid_mismatch"]:
        problems.append("transaction id does not match the query")
    if response["qname"].rstrip(".") != name.rstrip("."):
        problems.append(f"echoed qname {response['qname']!r}")
    if response["rcode"] != 0:
        problems.append(f"rcode was {response['rcode_name']}")
    if response["ancount"] < 1:
        problems.append(f"ANCOUNT is {response['ancount']}")
    answered = [answer.get("ip") for answer in response.get("answers", []) if "ip" in answer]
    if not problems and answered != [ip]:
        problems.append(f"answer set was {answered}, expected [{ip}]")
    if problems:
        raise SmokeFailure(f"{channel} query for {name} failed: {'; '.join(problems)}\n{logs()}")


def check_serving(args, name, label, *, tcp=True):
    """Readiness plus a UDP answer and, for the first run, a TCP answer.

    TCP is only meaningful while something can be forwarded to: the fast path
    accelerates UDP, so a TCP query on the restart run would reach userspace and
    fail against the closed upstream.
    """
    logs = lambda: container_logs(args, name)  # noqa: E731 - read only on failure
    control = f"http://{HOST}:{args.control_port}"
    wait_for_http(f"{control}/healthz", args.timeout, logs)
    wait_for_http(f"{control}/readyz", args.timeout, logs)
    require_answer(
        udp_query(args.dns_port, args.name_to_query),
        args.name_to_query,
        FAKE_ANSWER_IP,
        f"UDP ({label})",
        logs,
    )
    if tcp:
        require_answer(
            tcp_query(args.dns_port, args.name_to_query),
            args.name_to_query,
            FAKE_ANSWER_IP,
            f"TCP ({label})",
            logs,
        )
    return logs


# --------------------------------------------------------------------------- #
# The flow
# --------------------------------------------------------------------------- #
def smoke(args):
    name = None
    with tempfile.TemporaryDirectory(prefix="doggy-smoke-") as root:
        # The container reads its config through this path, so the temp root the
        # host user owns has to be traversable by uid 65532 as well.
        os.chmod(root, 0o755)
        try:
            check_image_user(args, expected_user(args))
            with FakeUpstream(args.upstream_port) as upstream:
                name = start_container(
                    args, root, run_tag="first", upstream=f"{HOST}:{upstream.port}", xdp=args.xdp
                )
                check_serving(args, name, "forwarded")
                if upstream.requests == 0:
                    raise SmokeFailure(
                        "the fake upstream saw no traffic, so nothing was forwarded to it; "
                        f"logs:\n{container_logs(args, name)}"
                    )
                if args.xdp:
                    check_pinned_map(args, name)
                stop_container(args, name)
                name = None
            if not args.xdp:
                print(
                    f"[ok] {args.image}: declares {expected_user(args)}, ran with no capabilities, "
                    f"answered {args.name_to_query} over UDP and TCP, "
                    f"forwarded {upstream.requests} queries"
                )
                return
            # FakeUpstream has left its `with` block and closed its socket, so the
            # only way the second run can answer is from the pinned map.
            name = start_container(args, root, run_tag="second", upstream=f"{HOST}:{args.dead_port}", xdp=True)
            logs = check_serving(args, name, "pinned map", tcp=False)
            if REUSE_LOG_LINE not in container_logs(args, name):
                raise SmokeFailure(f"the second start did not report reusing the pinned map; logs:\n{logs()}")
            print(
                f"[ok] {args.image}: pinned DNS_CACHE survived a container replacement and replayed "
                f"{args.name_to_query} -> {FAKE_ANSWER_IP} with no upstream to forward to"
            )
        finally:
            if name:
                stop_container(args, name)
            pin_dir = os.path.dirname(DEFAULT_XDP_PIN_PATH)
            if args.xdp and os.path.isdir(pin_dir):
                shutil.rmtree(pin_dir, ignore_errors=True)


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #
def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--image", required=True, help="image tag to run")
    parser.add_argument("--runtime", default="docker", choices=("docker", "podman"))
    parser.add_argument("--xdp", action="store_true", help="also prove pinned-map reuse")
    parser.add_argument(
        "--interface",
        default="lo",
        help="XDP interface; loopback accepts XDP everywhere the fast path can run "
        "at all, and the smoke queries 127.0.0.1, which is on it (use the real NIC "
        "to exercise the production path)",
    )
    parser.add_argument("--dns-port", type=int, default=15353, help="DNS port for the smoke config")
    parser.add_argument("--control-port", type=int, default=19091, help="control-plane port")
    parser.add_argument("--upstream-port", type=int, default=15399, help="fake forward upstream")
    parser.add_argument("--dead-port", type=int, default=15398, help="port with nothing behind it")
    parser.add_argument("--name-to-query", default="example.com", help="name to ask for")
    parser.add_argument(
        "--expect-user",
        default="",
        help="user the image must declare (default: 65532:65532, or root with --xdp). "
        "CI smokes the XDP image without --xdp, so it passes root here",
    )
    parser.add_argument("--timeout", type=float, default=60.0, help="seconds to wait for readiness")
    parser.add_argument("--name", default="doggy-dns-smoke", help="container name prefix")
    args = parser.parse_args(argv)
    if min(args.dns_port, args.control_port, args.upstream_port, args.dead_port) < 1024:
        parser.exit(2, "every port the smoke uses must be unprivileged\n")
    if len({args.dns_port, args.control_port, args.upstream_port, args.dead_port}) != 4:
        parser.exit(2, "the smoke ports must be distinct\n")
    return args


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        ensure_ports_free([args.dns_port, args.control_port, args.upstream_port])
        smoke(args)
    except SmokeFailure as failure:
        print(f"[FAIL] {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
