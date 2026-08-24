"""Live end-to-end probe for the NATS ocrd daemon.

WHY THIS EXISTS: the daemon's transport change cannot be proven by a build --
only by a real server, a real image and a real reply. There is no nats client
library on this box, but the NATS client protocol is a trivial line protocol,
so speaking it directly is cheaper than installing a dependency and keeps the
probe honest: it asserts the exact bytes the route66 side will send.

Usage: python ocrd_nats_probe.py <host:port> <subject-prefix> <image-path>
"""

import base64
import json
import socket
import sys
import time


class Conn:
    """One NATS connection, enough of the protocol to do request/reply."""

    def __init__(self, host, port):
        self.s = socket.create_connection((host, port), timeout=120)
        self.buf = b""
        # One sid per subscription: NATS ignores a duplicate sid, so reusing
        # it silently drops the second subscription's messages.
        self.sid = 0
        info = self.line()
        assert info.startswith(b"INFO "), info
        self.info = json.loads(info[5:])
        # protocol:1 is the modern client handshake; verbose off means the
        # server does not +OK every publish, which would just be noise here.
        self.s.sendall(
            b'CONNECT {"verbose":false,"pedantic":false,"tls_required":false,'
            b'"name":"ocrd-probe","lang":"python","version":"0","protocol":1}\r\n'
        )

    def line(self):
        while b"\r\n" not in self.buf:
            chunk = self.s.recv(65536)
            if not chunk:
                raise RuntimeError("server closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def exact(self, n):
        while len(self.buf) < n + 2:
            chunk = self.s.recv(65536)
            if not chunk:
                raise RuntimeError("server closed")
            self.buf += chunk
        body, self.buf = self.buf[:n], self.buf[n + 2:]
        return body

    def request(self, subject, payload, inbox):
        # ONE SID PER SUBSCRIPTION. NATS silently ignores a SUB that reuses a
        # live sid, so a second request on a reused sid is never delivered and
        # the probe hangs looking like a daemon fault.
        self.sid += 1
        self.s.sendall(b"SUB %s %d\r\n" % (inbox.encode(), self.sid))
        head = b"PUB %s %s %d\r\n" % (subject.encode(), inbox.encode(), len(payload))
        self.s.sendall(head + payload + b"\r\n")
        while True:
            line = self.line()
            if line.startswith(b"PING"):
                self.s.sendall(b"PONG\r\n")
                continue
            if line.startswith(b"MSG "):
                parts = line.split()
                body = self.exact(int(parts[-1]))
                # Match the reply to THIS request by its inbox subject. A
                # duplicate daemon on the same prefix answers too, and taking
                # whatever arrived next would attribute its reply to the wrong
                # request -- which is exactly how a stale daemon hides.
                if parts[1].decode() == inbox:
                    return body
            if line.startswith(b"-ERR"):
                raise RuntimeError(line.decode())


def main():
    hostport, prefix, image_path = sys.argv[1], sys.argv[2], sys.argv[3]
    host, port = hostport.rsplit(":", 1)
    c = Conn(host, int(port))
    print("server max_payload:", c.info.get("max_payload"))

    ver = c.request(prefix + ".ocr.version", b"", "_INBOX.probe.v")
    print("version reply:", ver.decode())

    raw = open(image_path, "rb").read()
    req = json.dumps(
        {
            "image": base64.b64encode(raw).decode(),
            "psm": 6,
            "lang": "eng",
            "dpi": 300,
            "upscale": 2,
            "pixel_budget": 1230000,
        }
    ).encode()
    print("request bytes:", len(req))
    t0 = time.time()
    reply = c.request(prefix + ".ocr.read", req, "_INBOX.probe.r")
    obj = json.loads(reply)
    print("read reply in %.2fs error=%r text[:120]=%r" % (
        time.time() - t0, obj["error"], obj["text"][:120]))
    assert set(obj) == {"text", "error"}, obj.keys()

    # THE FAILURE PATH IS PART OF THE CONTRACT: a request the daemon cannot
    # serve must still come back as a reply with a non-empty error. A silently
    # dropped request reads exactly like a passing check at the caller.
    bad = c.request(
        prefix + ".ocr.read",
        json.dumps({"image": "bm90LWFuLWltYWdl"}).encode(),
        "_INBOX.probe.bad",
    )
    print("bad-image reply:", bad.decode()[:160])
    assert json.loads(bad)["error"], "failed read produced no error"


main()
