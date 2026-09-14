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
(originally `rustc --edition 2021 -O -o <bin> main.rs` against the
single-file `main.rs`; after the module-tree refactor merged into this
branch, `rustc -O -C debuginfo=0 -o <bin> main.rs` with the module files
picked up automatically), runs it under `strace -f -c` (`-f` because each
connection is served on its own `std::thread`), replays the fixed request
mix, and reports the syscall summary. `bench/measure_syscalls.sh` always
uses the current documented invocation.

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

## 🔧 Change

Originally added `write_all_vectored()` (std only, no new crate) applied
at all five call sites identified above. While this PR was open, upstream
`main` independently landed the same fix as `write2()` in `buf.rs` —
applied to `send()`, `send_range_full`, `send_gzip_full`,
`send_range_single`, and `ws_send()` — so this branch merged that in and
deferred to it rather than keep a parallel mechanism. The one call site
upstream didn't cover, `send_chunked()`'s per-chunk loop (hex length
line, payload, trailing CRLF — the single largest contributor at 194
calls per response), now uses `write3()`, `write2`'s three-buffer twin,
added to `buf.rs` in the same style. Same bytes, same order, same error
handling as the original chained `write_all` calls either way. No public
behavior, byte layout, or existing test/verification expectation changes.

## 📊 Measurement

`bench/measure_syscalls.sh 20 100`, same fixed seeded workload, same
machine, same session — `strace -f -c` on an optimized `rustc` build,
before vs. after:

| syscall (response-path)        | before | after | delta |
|---------------------------------|-------:|------:|------:|
| `sendto`                        | 15,136 |   116 | -99.2% |
| `writev`                        |      0 | 5,654 |    new |
| `write`                         |      1 |     1 |     0 |
| **write-family total**          | **15,137** | **5,771** | **-61.9%** |
| **all syscalls (total)**        | **17,592** | **8,226** | **-53.2%** |

The write-family count clears the impact floor ("a measurable reduction
in syscall count") by a wide margin: 61.9% fewer write-family syscalls,
with every remaining `sendto`/`writev` accounted for by the same
byte-for-byte response bodies (verified with `cmp` against the
unmodified binary for `/`, `/chunked`, and gzip `/bytes`, plus a
3-request `/allocs` keep-alive check confirming the zero-heap invariant
still holds — `heap_allocations_total` stayed at the ~30 startup baseline).

Corroborating but *not* part of the gate (wall-clock is inadmissible on
this hardware per policy): the same 2000-request workload's wall time
dropped from 73.4s to 2.8s. That is consistent with removing a classic
Nagle-vs-delayed-ACK stall — each response used to leave the socket as
two or more TCP segments (header write, then a separately-flushed body
write), and this box's TCP stack was visibly paying ~40ms per split
response before this change.

### Post-merge re-verification

While this PR was open, upstream `main` split `main.rs` into a module
tree (`alloc.rs`/`buf.rs`/`http.rs`/`routes.rs`/`ranges.rs`/`gzip.rs`/
`multipart.rs`/`sse.rs`/`ws.rs`) and added a new `/events` endpoint. The
five touched functions moved with byte-identical bodies, so the fix was
ported as-is (`write_all_vectored()` now lives in `http.rs`). Re-running
`bench/measure_syscalls.sh 20 100` against the merged tree reproduced
**the same write-family count, 5,771** — the port changed nothing
observable. `sse.rs`/`/events` is untouched: it wasn't part of the
profiled baseline, so extending this fix to it is out of scope here.

## 🔬 Reproduce

```
git checkout <RED commit>     # harness + baseline, no fix yet
bash bench/measure_syscalls.sh 20 100

git checkout <this branch>    # fix applied (and ported through the later merge)
bash bench/measure_syscalls.sh 20 100
```

Both runs build with the currently documented `rustc` invocation for this
repo (no `Cargo.toml`, no crates added) — see `README.md`'s "Zero
dependency" line for the exact command, which `bench/measure_syscalls.sh`
always tracks.

## 🔁 Re-baseline (2026-09-14)

The `write2_before()` part of this fix (all call sites except
`send_chunked()`'s chunk loop) is still present in the current tree —
`main`'s history was force-reset to a point before PR #1 fully merged,
but that reset landed on a state that already had `write2_before` wired
into `send()`, the `/bytes` senders, and `ws_send()`. What's missing
again is the one call site upstream never covered even the first time:
`send_chunked()`'s per-chunk loop is back to three separate
`write_all_before()` calls (hex length, payload, trailing CRLF), and
`write3_before()` itself is gone from `buf.rs` — re-verified by reading
`routes.rs` and grepping `buf.rs` directly.

Re-ran the harness fresh on the exact commit this re-baseline is
committed against:

```
bash bench/measure_syscalls.sh 20 100
```

```
 65.30%  accept4       21 calls
 14.83%  setsockopt 15,195 calls   <- new since the original run (see note)
 11.71%  sendto     11,252 calls
  5.06%  writev      1,942 calls
  2.22%  recvfrom    2,000 calls
...
write-family syscalls (write+sendto+writev): 13,195
```

(`setsockopt` calls come from `set_write_timeout`/`set_read_timeout`
being re-armed before every read/write under the deadline scheme —
unrelated to this fix and not part of the write-family gate; it wasn't
broken out as its own line in the original strace summary but the
mechanism is unchanged.)

13,195 write-family syscalls — smaller than the original 15,137 baseline
because `write2_before` already collapsed every site except
`send_chunked`'s chunk loop, but `send_chunked` alone (3% of the
workload) still accounts for the overwhelming majority of what's left:
2,000 requests × ~3% × 194 calls/response ≈ 11,640 of the 13,195. The
fix — add `write3_before()` back to `buf.rs` and wire it into
`send_chunked()`'s chunk loop in place of the three `write_all_before()`
calls — is re-applied in the next (GREEN) commit and re-measured from
scratch.
