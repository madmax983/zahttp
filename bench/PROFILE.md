# zahttp response-path syscall profile

## 🎯 Workload

`bench/workload.py` drives a realistic, seeded (PRNG seed `1337`) request mix
against the real TCP entry point (`127.0.0.1:18080`), over keep-alive
connections, matching what a real client population would send:

| Route                          | Share |
|---------------------------------|------:|
| `GET /`                          | 35%   |
| `GET /health`                    | 15%   |
| `GET /headers`                   | 10%   |
| `POST /echo` (small form body)   | 12%   |
| `GET /bytes` (plain, 64 KiB)      | 10%   |
| `GET /bytes` (`Accept-Encoding: gzip`) | 6% |
| `GET /bytes` (single `Range`)     | 5%    |
| `POST /upload` (2-part multipart) | 4%    |
| `GET /chunked` (64-chunk stream)  | 3%    |

Reproduce:

```
bash bench/measure_syscalls.sh 20 100   # 20 connections x 100 requests = 2000
```

This builds the binary exactly as documented
(`rustc --edition 2021 -O -o <bin> main.rs`), runs it under `strace -f -c`
(`-f` because each connection is served on its own `std::thread`), replays
the fixed request mix, and reports the syscall summary.

## 📈 Profile (baseline, this repo's `main.rs` before any change)

```
% time     seconds  usecs/call     calls    errors syscall
------ ----------- ----------- --------- --------- ------------------
 62.50    0.765543       36454        21         1 accept4
 28.36    0.347395          22     15136           sendto
  7.73    0.094677          46      2020           recvfrom
  ...
100.00    1.224798          69     17592         2 total

write-family syscalls (write+sendto+writev): 15137
```

`sendto` (the syscall Rust's `TcpStream::write`/`write_all` issues on Linux)
is **86% of all syscalls made by the server** (15,136 of 17,592) — and this
is with `accept4` and `recvfrom` (the syscalls the workload actually forces
one-per-request-or-connection) held to 21 and 2,020 calls respectively. The
target — redundant `sendto` calls in the response-write path — is nowhere
close to the 5% profile floor; it *is* the profile.

## 💡 Hypothesis

Every response-sending function in `main.rs` builds a header buffer and a
body buffer separately (`Out` writes into two fixed stack arrays), then
issues **one `write_all` call per buffer**:

- `send()` (serves `/`, `/health`, `/headers`, `/echo`, `/upload`, `/metrics`,
  `/allocs`, `/time`, and all 4xx/5xx error paths): header write, body write
  — **2 `sendto` calls per response**.
- `send_range_full` / `send_gzip_full` / `send_range_single` (`GET /bytes`
  variants): same shape — **2 `sendto` calls per response**.
- `send_chunked()` (`GET /chunked`): 1 header write, then **3 separate
  `write_all` calls per chunk** (hex chunk-size line, payload, trailing
  `\r\n`) for all 64 chunks, plus the final `0\r\n\r\n` —
  **1 + 64×3 + 1 = 194 `sendto` calls per response**.

Mechanism: none of these call sites need separate syscalls — the pieces are
already sitting in independent buffers with well-known lengths at the point
of writing. `std::net::TcpStream` implements `Write::write_vectored` via a
single `writev(2)` call, so replacing back-to-back `write_all` calls with one
vectored write (looped only on a short write, via the stable
`IoSlice::advance_slices`) sends the exact same bytes in the exact same
order in one syscall instead of two or three.

Expected effect on this workload's syscall count, from the mix weights
above: `/chunked` is only 3% of requests but contributes roughly
0.03 × 194 ≈ 5.8 of the ≈7.6 average `sendto` calls per request — collapsing
its per-chunk 3-way write to 1 write drops it to 66 calls per response
(1 header + 64 chunks + 1 trailer), and collapsing the other routes' 2-way
write to 1 removes another call from the remaining 97% of requests.
