#!/usr/bin/env python3
"""Realistic-mix HTTP client for profiling zahttp's public TCP entry point.

Not a microbenchmark of any single function: this drives the same request
mix a real deployment would see (index/health checks, header echoes, small
POST bodies, range/gzip negotiation on /bytes, multipart uploads, a chunked
response stream) over keep-alive connections, exactly as a browser or a
load balancer's health checker would.

Usage:
    python3 bench/workload.py [--host HOST] [--port PORT]
                               [--connections N] [--requests-per-conn N]

Everything is seeded (PRNG seed 1337) so the exact same byte stream is sent
on every run - the workload is reproducible by anyone who checks out this
commit and runs it against a freshly built zahttp binary.
"""
import argparse
import random
import socket
import sys
import time

HOST_DEFAULT = "127.0.0.1"
PORT_DEFAULT = 18080

HEADERS_COMMON = (
    "User-Agent: zahttp-bench/1.0\r\n"
    "Accept: */*\r\n"
    "X-Request-Id: bench-0000000000\r\n"
)


def recv_response(sock: socket.socket, timeout: float = 5.0) -> bytes:
    """Read one full HTTP response (headers + body) off a keep-alive socket."""
    sock.settimeout(timeout)
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            return buf
        buf += chunk
    head_end = buf.index(b"\r\n\r\n")
    head = buf[:head_end].decode("latin-1")
    body = buf[head_end + 4 :]

    status_line = head.split("\r\n", 1)[0]
    status = int(status_line.split(" ", 2)[1])

    headers = {}
    for line in head.split("\r\n")[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()

    if headers.get("transfer-encoding", "").lower() == "chunked":
        while b"0\r\n\r\n" not in body:
            chunk = sock.recv(4096)
            if not chunk:
                break
            body += chunk
        return buf
    content_length = int(headers.get("content-length", "0"))
    while len(body) < content_length:
        chunk = sock.recv(4096)
        if not chunk:
            break
        body += chunk
    if status == 101:
        # Upgrade response has no framed body in this harness's requests.
        return buf
    return buf


def build_multipart(rng: random.Random) -> tuple[bytes, str]:
    boundary = "zabench-%08x" % rng.getrandbits(32)
    field_val = "hello-%d" % rng.randrange(1_000_000)
    file_body = bytes(rng.getrandbits(8) for _ in range(64))
    parts = []
    parts.append(f"--{boundary}\r\n".encode())
    parts.append(
        b'Content-Disposition: form-data; name="field1"\r\n\r\n'
        + field_val.encode()
        + b"\r\n"
    )
    parts.append(f"--{boundary}\r\n".encode())
    parts.append(
        b'Content-Disposition: form-data; name="file"; filename="a.bin"\r\n'
        b"Content-Type: application/octet-stream\r\n\r\n" + file_body + b"\r\n"
    )
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), boundary


def make_requests(rng: random.Random, n: int) -> list[bytes]:
    """Build n request byte-strings for one connection, in the traffic mix."""
    reqs = []
    for _ in range(n):
        pick = rng.random()
        if pick < 0.35:
            req = f"GET / HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}\r\n"
        elif pick < 0.50:
            req = f"GET /health HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}\r\n"
        elif pick < 0.60:
            req = (
                f"GET /headers HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}"
                "X-Extra-A: some-value\r\nX-Extra-B: another-value\r\n"
                "Accept-Language: en-US,en;q=0.9\r\n\r\n"
            )
        elif pick < 0.72:
            body = ("field=%d&note=benchmark-payload-line" % rng.randrange(1_000_000)).encode()
            req = (
                f"POST /echo HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}"
                f"Content-Type: application/x-www-form-urlencoded\r\n"
                f"Content-Length: {len(body)}\r\n\r\n"
            ).encode() + body
            reqs.append(req)
            continue
        elif pick < 0.82:
            req = f"GET /bytes HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}\r\n"
        elif pick < 0.88:
            req = (
                f"GET /bytes HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}"
                "Accept-Encoding: gzip\r\n\r\n"
            )
        elif pick < 0.93:
            start = rng.randrange(0, 65000)
            end = min(start + rng.randrange(100, 4000), 65535)
            req = (
                f"GET /bytes HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}"
                f"Range: bytes={start}-{end}\r\n\r\n"
            )
        elif pick < 0.97:
            mp_body, boundary = build_multipart(rng)
            req = (
                f"POST /upload HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}"
                f"Content-Type: multipart/form-data; boundary={boundary}\r\n"
                f"Content-Length: {len(mp_body)}\r\n\r\n"
            ).encode() + mp_body
            reqs.append(req)
            continue
        else:
            req = f"GET /chunked HTTP/1.1\r\nHost: localhost\r\n{HEADERS_COMMON}\r\n"
        reqs.append(req.encode())
    return reqs


def run_connection(host: str, port: int, requests: list[bytes]) -> int:
    sock = socket.create_connection((host, port), timeout=5.0)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    ok = 0
    try:
        for req in requests:
            sock.sendall(req)
            resp = recv_response(sock)
            if resp:
                ok += 1
    finally:
        sock.close()
    return ok


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default=HOST_DEFAULT)
    ap.add_argument("--port", type=int, default=PORT_DEFAULT)
    ap.add_argument("--connections", type=int, default=20)
    ap.add_argument("--requests-per-conn", type=int, default=50)
    ap.add_argument("--seed", type=int, default=1337)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    total_ok = 0
    total = args.connections * args.requests_per_conn
    t0 = time.monotonic()
    for c in range(args.connections):
        reqs = make_requests(rng, args.requests_per_conn)
        total_ok += run_connection(args.host, args.port, reqs)
    dt = time.monotonic() - t0
    print(f"requests sent: {total}  ok: {total_ok}  wall: {dt:.2f}s", file=sys.stderr)
    return 0 if total_ok == total else 1


if __name__ == "__main__":
    sys.exit(main())
