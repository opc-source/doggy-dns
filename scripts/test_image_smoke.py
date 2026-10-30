#!/usr/bin/env python3
"""Unit tests for the pure helpers in scripts/image_smoke.py.

The container work itself cannot be exercised here (it needs docker/podman), so
these cover the two pieces that decide pass/fail: the DNS query builder and the
response-header reader.
"""

import socket
import struct
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import image_smoke  # noqa: E402


def synthetic_response(txid, flags, qd, an, ns=0, ar=0, name=b"example.com"):
    """A response header plus the question section, as a real server sends it."""
    header = (
        txid.to_bytes(2, "big")
        + flags.to_bytes(2, "big")
        + qd.to_bytes(2, "big")
        + an.to_bytes(2, "big")
        + ns.to_bytes(2, "big")
        + ar.to_bytes(2, "big")
    )
    question = b"".join(len(l).to_bytes(1, "big") + l for l in name.split(b"."))
    question += b"\x00" + (1).to_bytes(2, "big") + (1).to_bytes(2, "big")
    return header + question


class BuildQueryTests(unittest.TestCase):
    def test_query_header_is_a_recursion_desired_a_question(self):
        packet = image_smoke.build_query("example.com", qtype=1)
        self.assertEqual(packet[0:2], b"\xbe\xef", "transaction id")
        flags = int.from_bytes(packet[2:4], "big")
        self.assertEqual(flags >> 15, 0, "QR must be 0 for a query")
        self.assertEqual(flags & 0x0100, 0x0100, "RD should be set")
        self.assertEqual(int.from_bytes(packet[4:6], "big"), 1, "QDCOUNT")
        self.assertEqual(int.from_bytes(packet[6:8], "big"), 0, "ANCOUNT")

    def test_query_encodes_the_name_as_labels_then_qtype_and_qclass(self):
        packet = image_smoke.build_query("a.b.example.com", qtype=1)
        expected_name = b"\x01a\x01b\x07example\x03com\x00"
        self.assertIn(expected_name, packet)
        self.assertTrue(packet.endswith(expected_name + b"\x00\x01\x00\x01"), packet[-8:])


class ReadResponseTests(unittest.TestCase):
    def test_reads_flags_counts_and_question_name(self):
        # 0x8180 = QR=1, AA=0, RD=1, RA=1, RCODE=0
        packet = synthetic_response(0xBEEF, 0x8180, qd=1, an=1)
        parsed = image_smoke.read_response(packet, 0xBEEF)
        self.assertTrue(parsed["is_response"])
        self.assertEqual(parsed["rcode"], 0)
        self.assertEqual(parsed["ancount"], 1)
        self.assertEqual(parsed["qname"], "example.com")

    def test_negative_rcodes_are_reported_not_raised(self):
        for rcode, label in ((2, "SERVFAIL"), (3, "NXDOMAIN")):
            # 0x8180 | rcode
            packet = synthetic_response(0xBEEF, 0x8180 + rcode, qd=1, an=0)
            parsed = image_smoke.read_response(packet, 0xBEEF)
            self.assertEqual(parsed["rcode"], rcode, label)
            self.assertEqual(parsed["rcode_name"], label)

    def test_a_mismatched_transaction_id_is_reported(self):
        packet = synthetic_response(0x1234, 0x8180, qd=1, an=1)
        parsed = image_smoke.read_response(packet, 0xBEEF)
        self.assertTrue(parsed["txid_mismatch"])

    def test_a_truncated_or_non_dns_reply_is_not_a_response(self):
        with self.assertRaises(image_smoke.NotDnsResponse):
            image_smoke.read_response(b"\x00\x01", 0xBEEF)


class ReadAnswersTests(unittest.TestCase):
    def test_parses_an_a_record_whose_owner_name_is_a_pointer(self):
        packet = synthetic_response(0xBEEF, 0x8580, qd=1, an=1)
        end_of_question = len(packet)
        # Owner name is the pointer to offset 12 (the question name), then
        # type/class/ttl/rdlength and a four-byte rdata.
        packet += b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 600, 4) + bytes([192, 0, 2, 1])
        answers = image_smoke.read_answers(packet, end_of_question, ancount=1)
        self.assertEqual(len(answers), 1)
        self.assertEqual(answers[0]["ip"], "192.0.2.1")
        self.assertEqual(answers[0]["ttl"], 600)

    def test_parses_an_a_record_whose_owner_name_is_written_out(self):
        packet = synthetic_response(0xBEEF, 0x8580, qd=1, an=1)
        end_of_question = len(packet)
        packet += b"\x07example\x03com\x00" + struct.pack("!HHIH", 1, 1, 30, 4) + bytes([127, 0, 0, 1])
        answers = image_smoke.read_answers(packet, end_of_question, ancount=1)
        self.assertEqual(answers[0]["ip"], "127.0.0.1")
        self.assertEqual(answers[0]["name"], "example.com")

    def test_a_non_a_record_has_no_ip_field(self):
        packet = synthetic_response(0xBEEF, 0x8580, qd=1, an=1)
        end_of_question = len(packet)
        rdata = b"\x03foo"
        packet += b"\xc0\x0c" + struct.pack("!HHIH", 16, 1, 60, len(rdata)) + rdata
        answers = image_smoke.read_answers(packet, end_of_question, ancount=1)
        self.assertEqual(answers[0]["rtype"], 16)
        self.assertNotIn("ip", answers[0])


class FakeUpstreamTests(unittest.TestCase):
    def test_replies_to_a_query_with_one_a_record(self):
        query = image_smoke.build_query("example.com", qtype=1, txid=0x1234)
        reply = image_smoke.fake_response(query, "192.0.2.1", ttl=600)
        parsed = image_smoke.read_response(reply, 0x1234)
        self.assertTrue(parsed["is_response"])
        self.assertEqual(parsed["rcode"], 0)
        self.assertEqual(image_smoke.read_answers(reply, parsed["question_ends_at"], parsed["ancount"])[0]["ip"], "192.0.2.1")

    def test_other_names_are_answered_nxdomain(self):
        query = image_smoke.build_query("other.test", qtype=1, txid=0x4321)
        reply = image_smoke.fake_response(query, "192.0.2.1", ttl=600, served_name="example.com")
        parsed = image_smoke.read_response(reply, 0x4321)
        self.assertEqual(parsed["rcode"], 3, "NXDOMAIN")
        self.assertEqual(parsed["ancount"], 0)

    def test_an_edns_query_gets_its_option_record_back_as_an_option_record(self):
        # hickory's forwarder sends EDNS0: an OPT record in the additional
        # section, which the real server's reply also carries. The fixture echoes
        # the question, so it has to echo the *question* and not the trailing
        # option bytes — a server that reads its answer section at the end of the
        # question would otherwise find type OPT where an A record belongs and
        # answer SERVFAIL ("record type OPT only allowed in additional section").
        query = bytearray(image_smoke.build_query("example.com", qtype=1, txid=0x2345))
        opt = b"\x00" + struct.pack("!HH", 41, 1232) + b"\x00\x00\x00\x00\x00\x00"
        query[10:12] = struct.pack("!H", 1)  # ARCOUNT = 1
        query += opt
        reply = image_smoke.fake_response(bytes(query), "192.0.2.1", ttl=600)
        parsed = image_smoke.read_response(reply, 0x2345)
        self.assertEqual(parsed["arcount"], 0, "the fixture must not echo the OPT record")
        answers = image_smoke.read_answers(reply, parsed["question_ends_at"], parsed["ancount"])
        self.assertEqual(answers[0]["rtype"], 1, "the first answer must be the A record")
        self.assertEqual(answers[0]["ip"], "192.0.2.1")

    def test_answers_the_same_query_over_tcp(self):
        # The container forwards a TCP client query over TCP as well, so the
        # fixture has to listen on both transports or the smoke's TCP assertion
        # would be testing doggy-dns's error path instead of its answer path.
        with image_smoke.FakeUpstream(0) as upstream:
            response = image_smoke.tcp_query(upstream.port, "example.com", timeout=5.0)
        self.assertEqual(response["rcode"], 0)
        self.assertEqual(response["answers"][0]["ip"], "192.0.2.1")


class EnsurePortsFreeTests(unittest.TestCase):
    def test_a_port_left_in_time_wait_is_not_reported(self):
        # A container the smoke stopped a second ago leaves its accepted
        # connection in TIME_WAIT on the listening port. That is not a collision —
        # the next run binds that port fine — and treating it as one made a
        # back-to-back re-run fail before anything started.
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind((image_smoke.HOST, 0))
        listener.listen(1)
        port = listener.getsockname()[1]
        client = socket.create_connection((image_smoke.HOST, port), timeout=2.0)
        server_side, _ = listener.accept()
        # The server closes first, so it is the one that keeps the connection in
        # TIME_WAIT afterwards.
        server_side.close()
        client.close()
        listener.close()
        image_smoke.ensure_ports_free([port])  # must not raise

    def test_a_port_taken_only_over_tcp_is_still_reported(self):
        # Both the fixture and the container answer over TCP as well as UDP, so a
        # precheck that only looked at UDP would let a collision surface much
        # later as an unexplained readiness timeout.
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind((image_smoke.HOST, 0))
        listener.listen(1)
        try:
            port = listener.getsockname()[1]
            with self.assertRaises(image_smoke.SmokeFailure) as caught:
                image_smoke.ensure_ports_free([port])
            self.assertIn("TCP", str(caught.exception))
        finally:
            listener.close()


class ExpectedUserTests(unittest.TestCase):
    """CI smokes the XDP image without `--xdp`, so the user cannot be derived from it."""

    def test_the_default_follows_the_variant(self):
        plain = image_smoke.parse_args(["--image", "doggy-dns:test"])
        self.assertEqual(image_smoke.expected_user(plain), "65532:65532")
        xdp = image_smoke.parse_args(["--image", "doggy-dns:test", "--xdp"])
        self.assertEqual(image_smoke.expected_user(xdp), "root")

    def test_an_explicit_expectation_wins_over_the_variant(self):
        args = image_smoke.parse_args(["--image", "doggy-dns:test", "--expect-user", "root"])
        self.assertEqual(image_smoke.expected_user(args), "root")


class ConfigTextTests(unittest.TestCase):
    """The `[xdp]` block the smoke writes decides whether reuse can be exercised."""

    def xdp_config(self, *extra):
        args = image_smoke.parse_args(["--image", "doggy-dns:test", "--xdp", *extra])
        return image_smoke.config_text(args, upstream="127.0.0.1:15399", xdp=True)

    def test_the_xdp_config_attaches_to_loopback_by_default(self):
        # The default-route interface is a real NIC, and a real NIC may not accept
        # XDP (the podman VM's virtio device does not). Loopback accepts it
        # everywhere the fast path can run at all, and a query to 127.0.0.1 —
        # what the smoke sends — is in the program's path.
        self.assertIn('interface = "lo"', self.xdp_config())

    def test_an_explicit_interface_wins(self):
        self.assertIn('interface = "eth0"', self.xdp_config("--interface", "eth0"))


if __name__ == "__main__":
    unittest.main()
