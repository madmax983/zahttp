# zahttp

A zero-dependency, zero-allocation HTTP/1.1 server. One file, `main.rs`.
`std` only — no Cargo project, no crates, no `extern` anything.

## The rules

- **Zero dependency:** compiled with `rustc --edition 2021 -O -o zahttp main.rs`.
  There is no `Cargo.toml`. There is nothing to `cargo add`.
- **Zero allocation (in our code):** no heap types, no `format!`, no `vec!`,
  no panicking helpers in the hot loop. Every byte lives in a fixed-size
  stack buffer (`[u8; 8192]` for the request head, `[u8; 4096]` for bodies)
  and parsing only ever borrows slices of that buffer. Errors become status
  codes, never panics.
- **The proof:** a counting global allocator backs the claim. `GET /allocs`
  reports total heap allocations since boot. Hit it five times over one
  keep-alive connection and watch the number not move:

```
$ curl -s localhost:18080/allocs localhost:18080/allocs localhost:18080/allocs
heap_allocations_total 30
heap_allocations_total 30
heap_allocations_total 30
```

The ~30 baseline is process startup and per-connection thread spawn —
`std`'s business. The per-request delta is exactly **0**.

## Endpoints

| Route        | Method | Notes                                              |
|--------------|--------|----------------------------------------------------|
| `/`          | GET/HEAD | a little HTML landing page                       |
| `/health`    | GET/HEAD | `ok`                                             |
| `/time`      | GET    | current time as an IMF-fixdate, hand-computed from the unix clock (Hinnant's civil-date algorithm, integer math, zero padding by hand) |
| `/headers`   | GET    | echoes your request headers back                   |
| `/echo`      | POST   | echoes the request body back (4 KiB cap → `413`); accepts `Content-Length` **or** `Transfer-Encoding: chunked` bodies |
| `/chunked`     | GET    | streams 64 generated chunks with `Transfer-Encoding: chunked` (no `Content-Length`) |
| `/metrics`   | GET    | request counter                                    |
| `/allocs`    | GET    | heap allocation counter (the proof)                |
| `/bytes`     | GET/HEAD | 64 KiB deterministic body with RFC 7233 range support (see below) |
| `/upload`    | POST   | parses `multipart/form-data` bodies, returns a part summary (see below) |

Also: proper `Date`/`Server`/`Content-Length`/`Connection` headers,
HTTP/1.0 + 1.1 keep-alive with pipelined-byte shifting, `400`/`404`/`405`/
`413`/`431`/`505` as appropriate, and a hand-rolled decimal printer
(`push_u64`) because `format!` was disqualified.

## Conditional requests (the dare, 2026-09-13)

`GET`/`HEAD /bytes` honors `If-None-Match` per RFC 7232:

- A matching validator short-circuits everything — including `Range`
  — with `304 Not Modified`, ETag echoed, no body, no `Content-Length`
  (a 304 never carries a body, so framing stays unambiguous and
  keep-alive survives).
- Weak comparison: `W/"zahttp-bytes-v1"` matches; `*` matches any
  current representation; comma-separated lists are scanned with a
  plain index loop (no allocation).
- Routes without an ETag (`/health`, etc.) simply ignore the header.
- Verification: 13 checks green — exact/weak/wildcard/list matches,
  stale etag → 200, match+Range → 304, header-name case-insensitivity,
  and a 304 followed by another request on the same keep-alive
  connection. `/allocs` delta **0** across 25 conditional 304s.

## Expect: 100-continue (the dare, 2026-09-13)

Clients may send `Expect: 100-continue` and wait for the interim
`HTTP/1.1 100 Continue` before streaming the body (RFC 7231 5.1.1):

- The expectation is scanned from the raw head *before* any body byte
  is read, so a client that truly waits never hangs.
- The interim response is sent only when a body is actually coming
  (chunked, or `Content-Length > 0`); a bodyless `GET` with the header
  just proceeds. Oversize bodies still fail fast with `413` and never
  see a 100. Unknown expectations (`Expect: pizza`) fail fast with
  `417 Expectation Failed`.
- HTTP/1.0 requests ignore the header entirely (an interim 100 would
  corrupt their framing).
- Verification: 11 protocol checks green — including wait-then-send,
  send-anyway pipelining, chunked bodies, and 100-continue on the
  second request of a keep-alive connection. `/allocs` delta **0**
  across 20 expect-uploads on one connection.

## Multipart form parsing (the dare, 2026-09-13)

`POST /upload` with `Content-Type: multipart/form-data; boundary=...`
parses the body per RFC 7578 and returns a plain-text summary:

```
parts: 2
part 0: name="field1" filename="-" type="text/plain" size=6
part 1: name="file" filename="a.txt" type="text/plain" size=12
```

- Byte-level parser over the borrowed request body: `memfind`/`at`
  helpers, no copies. Part descriptors (`name`, `filename`,
  `content_type`, body slice) live in a fixed `[FormPart; 16]` — all
  strings borrow from the body buffer.
- `boundary=` is extracted with a parameter scanner that respects
  quoting and requires parameter boundaries, so `name=` never matches
  inside `filename=` (tested both orders). Quoted boundaries work;
  backslash escapes inside quoted values are *not* processed
  (documented simplification, like the naive boundary search).
- Limits: 16 parts max (`413` beyond), 128-byte boundary cap, and the
  usual 4 KiB body cap. Missing/non-multipart content type, garbage
  framing, or a truncated body (no closing `--boundary--`) → `400`.
  Works with `Content-Length` and chunked request bodies alike.
- Verification: 18 protocol checks green, and `/allocs` moved **0**
  across 30 five-part uploads on one keep-alive connection.

## Range requests (the dare, 2026-09-13)

`GET`/`HEAD /bytes` serves a fixed 64 KiB deterministic body (LCG bytes,
computed once into a `LazyLock` — the initializer runs a single time and
involves no allocator) with full RFC 7233 semantics:

- **Single range** (`bytes=10-19`) → `206` with `Content-Range: bytes 10-19/65536`.
- **Open-ended** (`bytes=10-`) → through the end of the representation.
- **Suffix** (`bytes=-16`) → the last 16 bytes; a suffix longer than the
  body returns the whole body (`0-65535`), per spec.
- **Multiple ranges** → `206` with `multipart/byteranges` and a fixed
  boundary. The `Content-Length` is precomputed by a `part_len` formula
  that derives every literal length from the same `const`s the writer
  uses, so the math and the bytes can never drift apart again (ask me
  how I know — the first version hand-counted
  `Content-Type: application/octet-stream` as 38 bytes; it is 40).
- **Unsatisfiable or malformed** (`bytes=70000-`, `bytes=20-10`,
  `bytes=abc`, non-`bytes` units, >8 ranges) → `416` with
  `Content-Range: bytes */65536`. A `first-byte-pos` past the end
  clamps (`bytes=65500-999999` → `65500-65535`).
- **`If-Range`** with the fixed ETag `"zahttp-bytes-v1"`: match → `206`,
  mismatch → the Range is ignored and you get `200` full-body.
- `Accept-Ranges: bytes` is advertised; `HEAD` returns headers only;
  `POST /bytes` → `405`.

Parsing is byte-level over the borrowed header value: checked `u64`
accumulation (overflow → `416`, never wrap), at most 8 ranges into a
fixed `[(u64,u64); 8]`. Verification: 52 protocol checks green —
including byte-exact multipart bodies against an independent Python LCG
reference — and `/allocs` moved **0** across 40 mixed range requests on
one keep-alive connection.

## Chunked transfer encoding (the dare, 2026-09-12)

Both directions, still zero-allocation:

- **Decoding requests:** `POST /echo` accepts `Transfer-Encoding: chunked`
  (case-insensitive, comma-separated tokens honored, chunked wins over
  `Content-Length` per RFC 7230 §3.3.3). A small state machine
  (`decode_chunked`) parses hex chunk sizes — extensions (`;foo=bar`)
  ignored, uppercase hex fine — streams chunk data into a fixed 4 KiB
  buffer, validates the CRLF after each chunk, and swallows trailers.
  Oversize decoded bodies → `413`; malformed framing → `400`. Buffer
  compaction reuses the read buffer *below* the request head (never
  clobbering it), so keep-alive cursor math stays exact — verified with
  a chunked POST pipelined ahead of a GET on one connection.
- **Encoding responses:** `GET /chunked` streams 64 chunks of deterministic
  LCG hex (`<hexlen>\r\n<payload>\r\n`, terminated `0\r\n\r\n`), no
  `Content-Length` header. Verified byte-for-byte with `curl --raw`.

The allocator proof covers the chunked path too: five chunked POSTs
(with extensions, uppercase hex, and trailers) on one connection moved
`/allocs` not at all.

## WebSocket upgrade (the dare, 2026-09-12)

`GET /ws` speaks RFC 6455 with no crates — including the crypto:

- **Handshake:** requires `Upgrade: websocket` and `Connection: ...,
  upgrade` (case-insensitive tokens, comma-separated lists honored),
  `Sec-WebSocket-Version: 13` (anything else → `426` with a
  `Sec-WebSocket-Version: 13` header), and a non-empty
  `Sec-WebSocket-Key`. The `Sec-WebSocket-Accept` is
  `base64(sha1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))` computed
  by a hand-rolled SHA-1 (~90 lines: Merkle–Damgård, 80 rounds, fixed
  `[u32; 80]` schedule on the stack) and a hand-rolled base64 encoder.
  Both verified against Python's `hashlib`/`base64`, including the RFC's
  own example key (`dGhlIHNhbXBsZSBub25jZQ==` →
  `s3pPLMBiTxaQ9kYGzzhZRbK+xOo=`).
- **Frame codec:** text/binary echo, fragmentation reassembly into a
  fixed 4 KiB message buffer (continuation state machine), ping → auto
  pong, close → echo close then drop. Client frames must be masked —
  unmasked frames get a `1002` after the payload is drained (draining
  first avoids an RST racing our close frame, so violations are
  observable, not silent). RSV bits set → `1002`; orphan continuation
  or a new message mid-fragment → `1002`; messages over 4 KiB → `1009`;
  16-bit and 64-bit extended lengths both handled. Non-GET methods on
  `/ws` → `405` like every other known path.
- Verified with a hand-rolled Python client: 18/18 checks green
  (handshake vector, echo, binary, 64-bit lengths, ping/pong,
  reassembly, close handshake + TCP teardown, all four protocol
  violations, all four handshake rejections, token case-insensitivity).
- The allocator proof covers the frame loop too: a session of 20
  echoes + 10 pings + a fragmented message + close moved `/allocs` by
  3 — the new connection's thread spawn only, 0 per message.

## Run it

```
./zahttp          # listens on 127.0.0.1:18080
```

One thread per connection, all parsing on the stack.

## Footnotes (honest ones)

- "Zero allocation" covers code we wrote. `std` may allocate when spawning
  the per-connection thread — that's outside the request path and visible
  in `/allocs` as connection-time bumps, not per-request ones.
- HTTP date math assumes the Gregorian calendar and a year < 10000.
  Good until the year 10000, which feels like someone else's problem.
