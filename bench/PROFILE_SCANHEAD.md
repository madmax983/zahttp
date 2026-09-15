# zahttp request-path instruction profile: three redundant head scans

## 🎯 Workload

Same harness, same fixed seeded (PRNG seed `1337`) realistic request mix as
`bench/PROFILE.md` / `bench/PROFILE_DECODED.md` — `bench/workload.py` driving
the real TCP entry point (`127.0.0.1:18080`) with the documented route mix
(index, health, headers, `/echo`, `/bytes` plain/gzip/range, `/upload`
multipart, `/chunked`), gated on **instructions retired (Ir)** via
`valgrind --tool=callgrind`, same as `PROFILE_DECODED.md`.

Reproduce:

```
bash bench/measure_instructions.sh 20 100   # 20 connections x 100 requests = 2000
```

## 📈 Profile (baseline, this branch before this change)

```
82,430,398 (100.0%)  PROGRAM TOTALS

24,256,539 (29.43%)  __memset_avx2_unaligned_erms  (libc)
18,585,616 (22.55%)  main::gzip::gzip_encode
 8,262,100 (10.02%)  main::routes::send_chunked
 7,143,469 ( 8.67%)  __memcpy_avx_unaligned_erms  (libc)
 2,775,290 ( 3.37%)  main::serve
 2,728,166 ( 3.31%)  main::http::is_chunked
 2,601,963 ( 3.16%)  main::http::content_length_of
 2,502,961 ( 3.04%)  main::http::expect_of
 2,127,174 ( 2.58%)  core::iter::traits::iterator::Iterator::try_fold
 1,889,383 ( 2.29%)  main::http::parse_head
 1,745,588 ( 2.12%)  core::str::converts::from_utf8
 ...
```

Two entries already looked at and ruled out of scope by prior sessions:
`gzip_encode` is a one-time `LazyLock` cost that doesn't scale with request
count (`PROFILE_DECODED.md`), and `__memset_avx2_unaligned_erms`'s remaining
cost is dominated by mandatory zero-inits of stack buffers that are actually
read in full (`main.rs`'s `bbuf`, the response-body scratch buffer used by
~72% of this workload's routes) — eliminating that would need `MaybeUninit`
(`unsafe`, which this agent must ask before adding) or a >50-line refactor
splitting every route onto its own right-sized buffer; flagged as a
follow-up, not attempted here.

The target here is the three functions `main::http::is_chunked`,
`main::http::content_length_of`, and `main::http::expect_of`: **9.51%** of
the profile combined (2,728,166 + 2,601,963 + 2,502,961 = 7,833,090 of
82,430,398 Ir), comfortably over the 5% floor.

## 💡 Hypothesis

`main.rs`'s `serve()` calls all three functions, in this order, on every
request, before the body is read: `is_chunked(head)` to pick chunked vs.
content-length framing, `expect_of(head)` to answer `100-continue`/`417`,
and (only when not chunked) `content_length_of(head)`. Each function
independently walks the **entire** header block from scratch with its own
`while !rest.is_empty() { ... windows(2).position(\r\n) ... }` loop, doing a
full byte-by-byte double-CRLF scan and a colon-position scan on every header
line, only to throw away every line whose name doesn't match the one header
it's looking for.

I believe this can be reduced because the mechanism is duplicated work, not
inherent work: `is_chunked` scans for `Transfer-Encoding`, `content_length_of`
scans for `Content-Length`, and `expect_of` scans for `Expect`, but all three
are scanning the *same* `head` byte slice, line by line, with the *same*
line-splitting and colon-finding logic — the header lines get walked up to
three times before `parse_head` (a fourth full walk) runs on the same bytes
a few lines later in `serve()`. Collapsing the first three into one pass
that recognizes all three header names in a single loop removes two of
those three redundant walks over every non-matching header line, with
`parse_head`'s separate walk (which builds the full `Header` array every
downstream handler needs) left untouched.

## 🔧 Change

Add `http::scan_head(head: &[u8]) -> HeadScan` — one pass over the header
lines that recognizes `Transfer-Encoding` (comma-token search for
`chunked`, continuing past non-matching `Transfer-Encoding` lines exactly
like `is_chunked` did), `Content-Length` (first occurrence only, same
`parse_usize` validation as `content_length_of`), and `Expect` (first
occurrence only, HTTP/1.0 short-circuit preserved, same
`100-continue`/other split as `expect_of`), returning all three results in
one `HeadScan { chunked, content_length, expect }` struct. `main.rs`'s
`serve()` calls `scan_head` once instead of `is_chunked` + `expect_of` +
`content_length_of`, and reads the three fields in the same order the three
calls happened before (chunked check, then the `Expect: Other` → 417 check,
then — only inside the `!chunked` branch, exactly as before —
`content_length` is unwrapped and its `Err` still turns into the same 400).
`is_chunked`, `content_length_of`, and `expect_of` are deleted (nothing else
calls them). Every line-matching rule, trim, and error code is copied
verbatim from the three originals; no public behavior, status code, or byte
layout changes.

## 📊 Measurement (baseline — see PROFILE_SCANHEAD.md's follow-up commit for after)

`bench/measure_instructions.sh 20 100`, same fixed seeded workload, same
machine, same session, run twice back-to-back to confirm determinism before
touching any code:

| counter | run 1 | run 2 |
|---|---:|---:|
| total instructions (Ir) | 82,430,398 | 82,430,398 |
| `main::http::is_chunked` | 2,728,166 | 2,728,166 |
| `main::http::content_length_of` | 2,601,963 | 2,601,963 |
| `main::http::expect_of` | 2,502,961 | 2,502,961 |

Identical to the instruction on both runs — this is the committed baseline
the fix (next commit) is measured against.

## 🔬 Reproduce

```
git checkout <this commit>     # baseline, no fix yet
bash bench/measure_instructions.sh 20 100
```
