# zahttp request-path instruction profile: the dead `decoded` memset

## 🎯 Workload

Same harness and fixed, seeded (PRNG seed `1337`) request mix as
`bench/PROFILE.md` — `bench/workload.py` driving the real TCP entry point
(`127.0.0.1:18080`) with the realistic route/method mix documented there
(index, health, headers, `/echo`, `/bytes` plain/gzip/range, `/upload`
multipart, `/chunked`). None of that mix sends a request with
`Transfer-Encoding: chunked` — like almost all real HTTP/1.1 traffic, every
request body is framed with `Content-Length` (or has no body at all).

This time the deterministic counter is **instructions retired (Ir)**,
gated with `valgrind --tool=callgrind` instead of `strace`, via the new
`bench/measure_instructions.sh`.

Reproduce:

```
bash bench/measure_instructions.sh 20 100   # 20 connections x 100 requests = 2000
```

This builds the binary exactly as documented (`rustc -O -C debuginfo=0 -o
<bin> main.rs`), runs it under `valgrind --tool=callgrind` (a single
process — zahttp never execs a child, so no `--trace-children` is
needed), replays the fixed request mix, and reports the total Ir plus a
`callgrind_annotate` breakdown by self cost.

## 📈 Profile (baseline, this branch before this change)

```
71,032,993 (100.0%)  PROGRAM TOTALS

18,585,616 (26.16%)  main::gzip::gzip_encode
15,476,263 (21.79%)  __memset_avx2_unaligned_erms  (libc)
 8,239,712 (11.60%)  main::routes::send_chunked
 5,492,913 ( 7.73%)  __memcpy_avx_unaligned_erms  (libc)
 2,744,064 ( 3.86%)  main::serve
 2,728,166 ( 3.84%)  main::http::is_chunked
 2,601,963 ( 3.66%)  main::http::content_length_of
 2,502,961 ( 3.52%)  main::http::expect_of
 ...
```

`gzip_encode` is the single largest entry, but it is a `LazyLock`
one-time cost (the 64 KiB `/bytes` gzip representation is compressed
exactly once, on the first request that needs it, and every later
response borrows the cached result) — it does not scale with request
count and is inherent DEFLATE work besides, so it is out of scope here
(same reasoning `bench/PROFILE.md` already applied to `sse.rs`). It is
not touched by this change.

The target is `__memset_avx2_unaligned_erms` — **21.79% of the entire
profile**, comfortably over the 5% floor — with none of it attributable
to any single named zahttp function because `callgrind_annotate` charges
libc's memset to itself, not its call site. Reading `main.rs`'s `serve()`
against the request mix explains it:

```rust
let mut decoded = [0u8; BODY_CAP];   // BODY_CAP = 4096, unconditional
...
if chunked {
    ...
    body = &decoded[..dlen];
} else {
    ...
    body = &buf[body_start..consumed];   // decoded never touched
}
```

`decoded` is a 4096-byte stack array, zero-initialized on *every* request
regardless of whether the request body is chunked — and in this workload
(and in the overwhelming majority of real HTTP/1.1 traffic, which frames
bodies with `Content-Length`), it never is. Every one of the 2000
requests in this run pays for zeroing 4 KiB it never reads or writes.

## 💡 Hypothesis

`__memset_avx2_unaligned_erms` accounts for 21.79% of instructions, and
`main.rs`'s only unconditional stack-array zero-init of that size is
`decoded`. I believe it can be reduced because the zero-fill of `decoded`
is dead work on every non-chunked request: nothing reads `decoded` unless
`chunked` is true, so the memset only needs to happen inside the
`if chunked` branch. Moving the initializer there (deferred
initialization: `let mut decoded: [u8; BODY_CAP];` declared unconditionally,
assigned `[0u8; BODY_CAP]` only where the `chunked` branch is entered)
means LLVM only emits the memset on a path that is actually reachable in
this workload's 2000 requests: zero times.

## 🔧 Change

`main.rs`'s `serve()`: `decoded`'s declaration and its zero-initializer
are split. The array is declared without a value; `decoded = [0u8;
BODY_CAP]` moves to the first line inside `if chunked { ... }`, right
before it is passed to `decode_chunked()`. `decoded` is never read or
written outside that branch (the `else` branch builds `body` from `buf`
directly), so this is a pure reordering of when the same zero-fill runs,
not a behavior change: a chunked request body is decoded into exactly the
same zeroed-then-filled buffer as before, byte for byte. Verified with
`cmp` against the unmodified binary for `/`, `/bytes` (plain, gzip),
`/chunked`, `POST /echo` with `Content-Length`, and `POST /echo` with
`Transfer-Encoding: chunked` (the one path that still executes the
zero-init) — all byte-identical modulo the `Date:` header.

No public behavior change, no new dependency, no `unsafe`.

## 📊 Measurement

`bench/measure_instructions.sh 20 100`, same fixed seeded workload, same
machine, same session — `valgrind --tool=callgrind` Ir, before vs. after:

| counter                                    |     before |      after |    delta |
|---------------------------------------------|-----------:|-----------:|---------:|
| total instructions (Ir)                      | 71,032,993 | 62,799,003 | **-11.59%** |
| `__memset_avx2_unaligned_erms` (self cost)   | 15,476,263 |  7,250,263 | **-53.16%** |

Both counters clear the impact floor ("≥5% reduction in instruction
count on a benchmark that represents ≥5% of realistic workload cost") by
a wide margin. The removed instruction count (8,233,990) lines up almost
exactly with the mechanism: 2000 requests × 4096 bytes (`BODY_CAP`) =
8,192,000 bytes no longer zeroed, plus ~17 instructions of fixed call
overhead per eliminated memset. Every other line in the `callgrind_annotate`
breakdown (`gzip_encode`, `send_chunked`, `memcpy`, `is_chunked`,
`content_length_of`, `expect_of`, `parse_head`, ...) is byte-for-byte
unchanged between the two runs — this change touches nothing else on the
request path.

Corroborating but *not* part of the gate (wall-clock is inadmissible on
this hardware per policy): both runs report `requests sent: 2000  ok:
2000`, i.e. no functional regression at the harness level either.

`clippy-driver --edition 2021 -O main.rs` reports the same 15
pre-existing warnings, none new or removed, on both sides of the change.

## 🔬 Reproduce

```
git checkout <RED commit>     # harness + this baseline, no fix yet
bash bench/measure_instructions.sh 20 100

git checkout <GREEN commit>   # fix applied
bash bench/measure_instructions.sh 20 100
```

Both runs build with the currently documented `rustc` invocation for this
repo (no `Cargo.toml`, no crates added).
