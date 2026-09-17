# zahttp request-path instruction profile: byte-at-a-time hex-digit emission

## 🎯 Workload

Same harness, same fixed seeded (PRNG seed `1337`) realistic request mix as
`bench/PROFILE.md` / `bench/PROFILE_DECODED.md` / `bench/PROFILE_SCANHEAD.md`
— `bench/workload.py` driving the real TCP entry point (`127.0.0.1:18080`)
with the documented route mix (index, health, headers, `/echo`, `/bytes`
plain/gzip/range, `/upload` multipart, `/chunked`), gated on **instructions
retired (Ir)** via `valgrind --tool=callgrind`.

Reproduce:

```
bash bench/measure_instructions.sh 20 100   # 20 connections x 100 requests = 2000
```

## 📈 Profile (baseline, this branch before this change)

```
77,579,463 (100.0%)  PROGRAM TOTALS

24,246,984 (31.25%)  __memset_avx2_unaligned_erms  (libc)
18,585,616 (23.96%)  main::gzip::gzip_encode
 8,262,100 (10.65%)  main::routes::send_chunked
 7,143,557 ( 9.21%)  __memcpy_avx_unaligned_erms  (libc)
 2,998,599 ( 3.87%)  main::http::scan_head
 2,761,290 ( 3.56%)  main::serve
 2,127,174 ( 2.74%)  core::iter::traits::iterator::Iterator::try_fold
 1,889,383 ( 2.44%)  main::http::parse_head
 1,745,588 ( 2.25%)  core::str::converts::from_utf8
 ...
```

Re-ran the harness twice more to confirm determinism before picking a
target: 77,579,478 and 77,579,480 (a ~17-instruction, 0.00002% wobble,
consistent with the noise level `PROFILE_SCANHEAD.md` already documented
on this shared-vCPU machine).

Three entries already looked at and ruled out of scope by prior sessions:
`gzip_encode` is a one-time `LazyLock` cost that doesn't scale with request
count, `__memset_avx2_unaligned_erms`'s remaining cost is dominated by
mandatory zero-inits of stack buffers that are actually read in full
(would need `MaybeUninit`/`unsafe`, which this agent must ask before
adding, or a >50-line per-route-buffer refactor), and `scan_head` is
itself the result of the prior PR's fix.

The target here is `main::routes::send_chunked`: **10.65%** of the
profile (8,262,100 of 77,579,463 Ir), comfortably over the 5% floor —
despite `/chunked` being only 3% of the workload's request mix (~60 of
2000 requests), because each `/chunked` response emits 64 chunks and
every chunk does non-trivial per-byte work.

A source-annotated build (`rustc -O -g`, debug info only, same
optimization level, used for investigation only — not the gating build)
attributes `send_chunked`'s cost almost entirely to `buf.rs`'s
`push_hex_u64`, inlined into `send_chunked`:

```
4,890,966 ( 6.30%)  buf.rs:main::routes::send_chunked   (push_hex_u64's own lines, inlined)
1,321,066 ( 1.70%)  core::cmp:main::routes::send_chunked
1,291,486 ( 1.66%)  core::slice::index:main::routes::send_chunked
```

Line-level annotation of `buf.rs::push_hex_u64` shows the cost sits in two
loops:

```rust
pub(crate) fn push_hex_u64(o: &mut Out, v: u64) {
    let mut tmp = [0u8; 16];
    let mut n = 0usize;
    if v == 0 {
        tmp[0] = b'0';
        n = 1;
    } else {
        let mut x = v;
        while x > 0 && n < tmp.len() {       // 1,164,350 + 1,035,300 + 244,006 Ir
            tmp[n] = HEX[(x & 15) as usize];
            x >>= 4;
            n += 1;
        }
    }
    while n > 0 {                             // 488,012 + 244,006 Ir
        n -= 1;
        o.push(&tmp[n..n + 1]);                // one Out::push call PER HEX DIGIT
    }
}
```

## 💡 Hypothesis

`push_hex_u64` already builds the complete hex-digit sequence into a local
`tmp` buffer (LSB-first, since digits are produced by successive `x & 15`
extractions), but then re-emits it one byte at a time via
`o.push(&tmp[n..n+1])` in the second loop — up to 16 separate calls to
`Out::push` per hex number, each paying its own bounds check
(`saturating_sub`, `.min()`), its own 1-byte `copy_from_slice`, and its own
overflow-flag branch, instead of one bounds check and one `copy_from_slice`
for the whole digit sequence.

`send_chunked` calls `push_hex_u64` 4 times per chunk (for four random
`u64` RNG outputs, each almost always the full 16 hex digits since a
uniformly random 64-bit value has only a 1-in-16 chance of a leading zero
nibble) plus one `push_hex_usize` (which forwards to `push_hex_u64`) for
the chunk-size header — 5 hex-encode calls × 64 chunks × ~60 `/chunked`
requests in this workload ≈ 19,200 calls, each currently issuing on
average roughly a dozen redundant single-byte `Out::push` calls that a
single whole-slice `push` call would replace.

I believe this can be reduced because the mechanism is duplicated,
avoidable work, not inherent work: computing the digit count up front
(`64 - v.leading_zeros()` bit-width, rounded up to nibbles) lets every
digit be written directly into `tmp` in final (MSB-first) order in one
forward pass — no LSB-first scratch pass, no second reversal pass, and
exactly one `push(&tmp[..n])` call instead of up to 16.

## 🔧 Change (next commit)

Rewrite `buf.rs::push_hex_u64` to compute the digit count `n` up front via
`v.leading_zeros()` (keeping the `v == 0` special case, since
`leading_zeros(0) == 64` would otherwise make the bit-width formula
divide out to 0 digits), fill `tmp[0..n]` directly in MSB-first order with
a single forward loop, and call `o.push(&tmp[..n])` once. Every digit
value and the total digit count are unchanged for every input — verified
by hand for `v = 0, 1, 15, 16, 255, 256, u64::MAX` (matching every
power-of-16 boundary in the digit-count formula) — so output is
byte-identical for every caller (`routes.rs::send_chunked`'s 4
`push_hex_u64` calls and 1 `push_hex_usize` call, `sse.rs`'s
`push_hex_usize` call). `push_hex_usize` itself is untouched (it just
forwards to `push_hex_u64`).

## 📊 Measurement (recorded after the fix, next commit)

To be filled in by the GREEN commit from the same
`bench/measure_instructions.sh 20 100` harness, same machine, same
session.

## 🔬 Reproduce

```
git checkout <RED commit>      # this baseline, no fix yet
bash bench/measure_instructions.sh 20 100

git checkout <GREEN commit>    # push_hex_u64 fix applied
bash bench/measure_instructions.sh 20 100
```
