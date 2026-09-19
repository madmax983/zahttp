# zahttp response-path syscall profile: one syscall per /chunked chunk

## 🎯 Workload

Same harness, same fixed seeded (PRNG seed `1337`) realistic request mix as
`bench/PROFILE.md` — `bench/workload.py` driving the real TCP entry point
(`127.0.0.1:18080`) with the documented route mix (35% `/`, 15% `/health`,
10% `/headers`, 12% `POST /echo`, 10%/6%/5% `/bytes` plain/gzip/range, 4%
`POST /upload`, 3% `GET /chunked`), gated on **write-family syscall count**
(`write`+`sendto`+`writev`) via `strace -f -c`.

Reproduce:

```
bash bench/measure_syscalls.sh 20 100   # 20 connections x 100 requests = 2000
```

## 📈 Profile (baseline, this branch before this change)

```
 64.86%  accept4       21 calls
 15.64%  writev     5,654 calls
 13.52%  setsockopt 7,771 calls
  4.33%  recvfrom   2,000 calls
  0.34%  sendto       116 calls
  ...
write-family syscalls (write+sendto+writev): 5771
all syscalls (total): 15976
```

Re-ran once more to confirm determinism before picking a target: identical
5,771 write-family / 15,976 total on both runs (this counter, unlike Ir on
this shared-vCPU box, has shown zero wobble across every prior PR that
gated on it — see `bench/PROFILE.md`).

`main::routes::send_chunked` (`GET /chunked`, 3% of the workload mix, ~60
of 2000 requests) is the target: `bench/PROFILE.md`'s prior fix collapsed
its per-chunk write from 3 `write_all_before()` calls (hex-length line,
payload, trailing CRLF) down to 1 `write3_before()` call, but never
collapsed *across* chunks. Each `/chunked` response still issues
1 (header) + 64 (one `writev` per chunk) + 1 (terminator) = **66
write-family syscalls**. At ~60 requests that's 60 × 66 = 3,960 — **68.7%
of all 5,771 write-family syscalls in the entire 2000-request workload**,
from a route that is 3% of traffic. `setsockopt` tracks 1:1 with every
read and every write (`read_before`/`write_all_before`/`write2_before`/
`write3_before` each re-arm the socket timeout before the call), so it
falls by the same amount for free.

## 💡 Hypothesis

`send_chunked()` builds each chunk's hex-length line and payload into
small per-iteration stack buffers (`cbuf`/`pbuf`) and immediately flushes
them with their own `write3_before()` call, 64 times in a row, with no
delay between iterations — unlike `sse.rs`'s `/events`, which paces one
frame per second via `thread::sleep` and genuinely needs one write per
frame, `/chunked` generates and writes all 64 chunks back-to-back in a
tight loop. Nothing about chunked-transfer-encoding framing requires a
socket syscall per chunk: the wire format is just bytes
(`<hex-len>\r\n<data>\r\n` repeated, terminated by `0\r\n\r\n`), and TCP
segmentation is independent of how many `write()` calls produced the
bytes. I believe the 64 per-chunk writes can be collapsed into the same
"one `writev` per response" shape every other route already uses
(`buf::write2_before`, per `bench/PROFILE.md`'s prior dare) by
accumulating all 64 chunks plus the terminator into one larger stack
buffer first, then issuing exactly one `write2_before(header, body)` call
for the entire response — same bytes, same order, one syscall instead of
66.

## 🔧 Change

(applied in the next commit) Add a `CHUNKED_BODY_CAP` stack buffer sized
for the worst case (64 chunks × ≤84 bytes + 5-byte terminator = 5,381
bytes worst case, with headroom), rewrite `send_chunked()` to build every
chunk's `<hex-len>\r\n<payload>\r\n` directly into that shared buffer
instead of a fresh per-chunk buffer flushed immediately, append the
`0\r\n\r\n` terminator to the same buffer, and replace the header write +
64 `write3_before()` calls + terminator write with one
`write2_before(header, body)` call. Remove `write3_before()` from
`buf.rs`, its only caller.

## 📊 Measurement

Baseline recorded above. After-numbers and the delta will be recorded in
the next (GREEN) commit once the fix lands, from the same harness, same
machine, same session.

## 🔬 Reproduce

```
git checkout <this commit>      # RED: baseline only, no fix yet
bash bench/measure_syscalls.sh 20 100

git checkout <next commit>      # GREEN: fix applied
bash bench/measure_syscalls.sh 20 100
```
