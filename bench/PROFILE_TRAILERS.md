# zahttp request-path instruction profile: TrailerStore zeroed twice per request

## 🎯 Workload

Same harness, same fixed seeded (PRNG seed `1337`) realistic request mix as
`bench/PROFILE.md` / `bench/PROFILE_DECODED.md` / `bench/PROFILE_SCANHEAD.md` /
`bench/PROFILE_HEXPUSH.md` / `bench/PROFILE_CHUNKBATCH.md` — `bench/workload.py`
driving the real TCP entry point (`127.0.0.1:18080`) with the documented route
mix (35% `/`, 15% `/health`, 10% `/headers`, 12% `POST /echo`, 10%/6%/5%
`/bytes` plain/gzip/range, 4% `POST /upload`, 3% `GET /chunked`), gated on
**instructions retired (Ir)** via `valgrind --tool=callgrind`.

Reproduce:

```
bash bench/measure_instructions.sh 20 100   # 20 connections x 100 requests = 2000
```

## 📈 Profile (baseline, this branch before this change)

```
=== callgrind Ir summary (2000 requests, seed 1337) ===
I   refs:      68,849,777

=== top self-cost functions ===
24,604,322 (35.74%)  __memset_avx2_unaligned_erms  (libc)
18,585,616 (26.99%)  main::gzip::gzip_encode
 3,835,482 ( 5.57%)  main::routes::send_chunked
 3,620,697 ( 5.26%)  __memcpy_avx_unaligned_erms  (libc)
 2,998,599 ( 4.36%)  main::http::scan_head
 2,761,290 ( 4.01%)  main::serve
 2,127,174 ( 3.09%)  core::iter::traits::iterator::Iterator::try_fold
 1,889,383 ( 2.74%)  main::http::parse_head
 1,745,588 ( 2.54%)  core::str::converts::from_utf8
...
```

Re-ran the harness three times to confirm determinism before picking a
target: 68,849,537 / 68,849,993 / 68,850,078 / 68,849,777 — a ~540-instruction,
0.0008% wobble, consistent with the noise level prior `PROFILE_*.md` files in
this repo already documented on this shared-vCPU machine.

Two entries already looked at and ruled out of scope by prior sessions:
`gzip_encode` is a one-time `LazyLock` cost that doesn't scale with request
count (`PROFILE_DECODED.md`), and `send_chunked` was already the target of
`PROFILE_HEXPUSH.md`/`PROFILE_CHUNKBATCH.md`.

`__memset_avx2_unaligned_erms` is **35.74% of the entire profile** — the
single largest line — with none of it attributable to a named zahttp
function because `callgrind_annotate` charges libc's memset to itself, not
its call site. A source-annotated build (`rustc -O -g`, debug info only,
same optimization level, used for investigation only — not the gating
build) attributes the memset cost to its call sites and finds something
prior sessions' `bbuf`-focused note (`PROFILE_SCANHEAD.md`,
`PROFILE_HEXPUSH.md`) didn't have visibility into: two *separate*, equally
expensive call sites, each `TrailerStore::empty()` (a 4,232-byte struct: a
16×256-byte `lines` array plus a 16-entry `lens` array), each costing
**8,498,000 Ir across exactly 2,000 calls** (one per request):

```
-- http.rs line 649 (TrailerStore::empty(), called from main.rs::serve) --
  8,000 ( 0.01%)          TrailerStore {
8,498,000 (12.34%)  => __memset_avx2_unaligned_erms (2,000x)
                             count: 0,
                             lines: [[0u8; TRAILER_LINE_MAX]; MAX_TRAILERS],
                             lens: [0usize; MAX_TRAILERS],
                         }

-- http.rs line 485 (TrailerStore::empty(), called from parse_head's own
   Request literal) --
 50,000 ( 0.07%)      Parse::Ready(Request {
8,498,000 (12.34%)  => __memset_avx2_unaligned_erms (2,000x)
                             method, target, version, headers,
                             header_count: count,
                             body: &[],
                             trailers: TrailerStore::empty(),
                         })
```

Combined: **16,996,000 Ir, 24.68% of the total 68,849,777-Ir profile** —
comfortably over the 5% floor, and larger than any single named function in
the profile including `gzip_encode`.

## 💡 Hypothesis

`main.rs::serve()` already builds one `TrailerStore::empty()` per request
(`let mut trailers = TrailerStore::empty();`, needed so the chunked-decode
branch has a mutable accumulator to hand `decode_chunked()`), then later
does `req.trailers = trailers;` after `parse_head()` returns — a
4,232-byte copy-assignment (`__memcpy_avx_unaligned_erms`, 850,000 Ir +
8,000 Ir self-cost across 2,000 calls, visible in the debug-info build at
main.rs's `req.trailers = trailers;` line).

But `parse_head()` (`http.rs`) builds its *own*, entirely separate
`TrailerStore::empty()` to satisfy the `trailers` field of the `Request`
struct literal it constructs and returns. That value is **never read**:
the one and only caller (`main.rs::serve()`) unconditionally overwrites it
one line later with `req.trailers = trailers;`, on every code path
(`Parse::Ready` is the only variant that reaches that line). So on every
single request, this program pays for a full 4,232-byte zero-init *twice*
plus a 4,232-byte copy — once inside `parse_head()` (entirely wasted:
built, then immediately discarded), once in `serve()` (the one that
matters), and once again to copy `serve()`'s copy over `parse_head()`'s.

Mechanism: thread the already-materialized `trailers` value from
`serve()` into `parse_head()` as a parameter, and have `parse_head()`
place it directly into the `Request` it constructs. This removes
`parse_head()`'s own throwaway `TrailerStore::empty()` (the dead
8,498,000-Ir zero-init) and the subsequent `req.trailers = trailers;`
copy-assignment (the 858,000-Ir memcpy) in the same stroke, since the value
is now moved into place once instead of built-then-discarded-then-copied.
The one remaining `TrailerStore::empty()` (`serve()`'s own, still needed
unconditionally as the chunked-decode accumulator, and moved into
`parse_head()` afterward) is inherent: eliminating it too would need
`MaybeUninit`/`unsafe` (this agent must ask before adding) since Rust
requires every field of a struct literal — including the array-backed
`lines`/`lens` — to hold *some* valid value, and this workload's fixed
mix never sends a chunked request body, so there is no cheaper "real
work" to fold this into instead.

## 🔧 Change

`http.rs`: `parse_head(head: &[u8])` → `parse_head(head: &[u8], trailers:
TrailerStore)`; the `Request` literal's `trailers: TrailerStore::empty()`
field becomes `trailers` (the parameter, moved in directly).

`main.rs`: `parse_head(head)` → `parse_head(head, trailers)` (passing the
already-built local by move); the now-redundant `req.trailers = trailers;`
line after the match is deleted (the value is already in place — `trailers`
was moved into `parse_head()`, so re-assigning it would no longer even
compile).

No behavior, byte layout, or existing verification expectation changes:
the final `req.trailers` value is byte-identical to before (previously
`serve()`'s copy always won via the unconditional overwrite; now that same
copy is placed directly, one move instead of build-then-discard-then-copy).

## 📊 Measurement

`bench/measure_instructions.sh 20 100`, same fixed seeded workload, same
machine, same session — `valgrind --tool=callgrind` Ir, before vs. after:

| metric                          |     before |     after |    delta |
|----------------------------------|-----------:|----------:|---------:|
| total instructions (Ir)          | 68,849,777 | 61,221,612 | **-11.08%** |
| `__memset_avx2_unaligned_erms` (Ir) | 24,604,322 | 16,115,877 | **-34.5%** |

Re-ran the after build twice to confirm determinism: 61,221,612 and
61,221,632 (a 20-instruction, 0.00003% wobble — tighter than the baseline's
own run-to-run noise).

The total-instruction delta (-11.08%) clears the impact floor ("≥5%
reduction in instruction count on a benchmark that represents ≥5% of
realistic workload cost" — this benchmark *is* the realistic workload) by
better than 2x, and the specific target the hypothesis named
(`TrailerStore::empty()`'s duplicate zero-init, 12.34% of the baseline
profile on its own) is fully removed: the memset reduction of 8,488,445 Ir
matches the predicted 8,498,000-Ir removal of exactly one of the two
identical constructor calls, within measurement noise.

Functional verification (this project has no unit test harness — see
`README.md` — so, matching prior PRs' method): built old and new binaries
from the same commit range and ran both against direct `curl` requests for
every route in the workload mix plus the ones outside it that exercise
this exact code path — `/`, `/health`, `/headers`, `POST /echo`, `GET
/bytes` (plain, gzip, and single-range), `GET /chunked`, `POST /upload`
(multipart), and `POST /trailers` with a real `Transfer-Encoding: chunked`
body carrying a trailer (`decode_chunked()`'s `&mut trailers` path, the one
non-trivial consumer of the value this change re-threads). All ten
response bodies were byte-identical (`cmp`) between the two binaries. A
3-request `/allocs` check on one keep-alive connection held flat
(`heap_allocations_total` unchanged across all three hits on the new
binary), confirming the zero-heap invariant still holds.

`clippy-driver --edition 2021 -O main.rs` reports the same pre-existing
warnings on both sides (all in `gzip.rs`/`sse.rs`/`ws.rs`, none in the two
files this change touches).

## 🔬 Reproduce

```
git checkout <RED commit>     # baseline, no fix yet
bash bench/measure_instructions.sh 20 100

git checkout <this commit>    # fix applied
bash bench/measure_instructions.sh 20 100
```

Both runs build with the currently documented `rustc` invocation for this
repo (no `Cargo.toml`, no crates added) — see `README.md`'s "Zero
dependency" line for the exact command, which `bench/measure_instructions.sh`
always tracks.
