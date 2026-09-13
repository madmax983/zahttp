// zahttp - a zero-dependency, zero-allocation HTTP/1.1 server.
//
// The rules of the game:
//   * std only. No Cargo project, no crates, no extern items. One rustc
//     invocation (`rustc -O -C debuginfo=0 -o zahttp main.rs`) builds the
//     whole module tree below.
//   * No heap in our code, ever: every byte lives in a fixed-size stack
//     buffer, and parsing only ever borrows slices of that buffer.
//   * Errors become status codes, never panics: the hot loop has no
//     panicking helpers.
//   * A counting global allocator backs the "we allocate nothing" claim:
//     hit /allocs twice over one connection and watch the number not move.
//     (Spawning the per-connection thread is std's business and may bump
//     the counter; the per-request path is ours and stays flat.)
//
// Module map:
//   alloc.rs     the counting global allocator (/allocs is the proof)
//   buf.rs       Out, the fixed-buffer writer; HTTP dates; integer parsing
//   dates.rs     HTTP-date parsing (RFC 7231 7.1.1.1) for conditionals
//   http.rs      request parsing, framing, expectations, the send() writer
//   routes.rs    INDEX, route(), OPTIONS, the small handlers
//   sse.rs       GET /events — the infinite Server-Sent Events feed
//   ranges.rs    GET/HEAD /bytes — RFC 7233 ranges, conditionals
//   gzip.rs      hand-rolled DEFLATE/gzip for /bytes content-encoding
//   multipart.rs POST /upload — RFC 7578 parsing, in place
//   ws.rs        GET /ws — RFC 6455 WebSocket, handshake + frame codec

mod alloc;
mod buf;
mod dates;
mod gzip;
mod http;
mod multipart;
mod ranges;
mod routes;
mod sse;
mod ws;

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::alloc::REQUEST_COUNT;
use crate::buf::{Out, BODY_CAP, READ_CAP, RESP_BODY_CAP};
use crate::http::{
    content_length_of, decode_chunked, expect_of, find_double_crlf, is_chunked, keep_alive,
    parse_head, path_of, read_before, send_100, Expect, Parse,
};
use crate::ranges::send_ranges;
use crate::routes::{route, send, send_chunked, send_options};
use crate::sse::send_events;
use crate::ws::ws_serve;

// ---- connection lifecycle -------------------------------------------------
// nginx-style knobs, enforced with atomics and deadline-bounded reads —
// no heap.
//
// HEAD_TIMEOUT_SECS is a TOTAL deadline for the request head, re-armed
// per request: pre-request silence, idle keep-alive gaps, and slow header
// dribbles all die here. BODY_TIMEOUT_SECS is a TOTAL deadline for the
// request body (content-length and chunked). Each read is bounded by the
// time left, so the old 1-byte-per-4.9-seconds slowloris loophole is
// closed: dribbles die at the deadline. Expiry closes the connection
// silently (a chunked timeout answers 400 like any other body error,
// then closes).
// KEEPALIVE_REQUESTS caps requests per connection; the last one is
// answered `Connection: close`, nginx-style.
// MAX_CONNECTIONS caps concurrent connections; over-cap connects get a
// bare `503 Service Unavailable` with no request read, then close.
const HEAD_TIMEOUT_SECS: u64 = 5;
const BODY_TIMEOUT_SECS: u64 = 5;
const KEEPALIVE_REQUESTS: u64 = 100;
const MAX_CONNECTIONS: usize = 128;
static ACTIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

// ---- connection driver --------------------------------------------------

fn serve(mut stream: TcpStream) {
    let mut buf = [0u8; READ_CAP];
    let mut n = 0usize;
    let mut served = 0u64;
    // Every read below goes through read_before() with a total deadline,
    // so the socket needs no standing timeout of its own.
    loop {
        // 1. read until the head is complete. Total deadline, re-armed per
        // request: idle gaps, pre-request silence, and slow header
        // dribbles all die here.
        let head_deadline = Instant::now() + Duration::from_secs(HEAD_TIMEOUT_SECS);
        let head_end: usize = loop {
            if let Some(p) = find_double_crlf(&buf[..n]) {
                break p;
            }
            if n == buf.len() {
                send(&mut stream, 431, "text/plain", b"head too large\n", false, true);
                return;
            }
            match read_before(&mut stream, &mut buf[n..], head_deadline) {
                Ok(0) => return,
                Ok(k) => n += k,
                Err(_) => return,
            }
        };
        // 2. framing: chunked wins over content-length (RFC 7230 3.3.3).
        // The head slice includes the trailing \r\n\r\n so the last header
        // line always has a line ending for the scanners below.
        let body_start = head_end + 4;
        let chunked = {
            let head = &buf[..head_end + 4];
            is_chunked(head)
        };
        // Deferred initialization: zero-filling BODY_CAP bytes here,
        // unconditionally, would cost every request a memset even though
        // `decoded` is only ever written to (and read from) inside the
        // `chunked` branch below. Declaring it without a value and
        // assigning `[0u8; BODY_CAP]` only on that branch means the
        // memset is emitted solely where it is reachable, so a
        // content-length body (the common case) never pays for it.
        let mut decoded: [u8; BODY_CAP];
        let body: &[u8];
        let consumed: usize;
        // 2b. expectations (RFC 7231 5.1.1): answered before any body byte
        // is read. Unknown expectations fail fast with 417; a 100-continue
        // is only ever sent when a body is actually coming.
        let expect = expect_of(&buf[..head_end + 4]);
        if expect == Expect::Other {
            send(&mut stream, 417, "text/plain", b"expectation failed\n", false, true);
            return;
        }
        // 2c. total body deadline, armed once the head is in: content-
        // length and chunked dribbles both die here.
        let body_deadline = Instant::now() + Duration::from_secs(BODY_TIMEOUT_SECS);
        if chunked {
            if expect == Expect::Continue && !send_100(&mut stream) {
                return;
            }
            decoded = [0u8; BODY_CAP];
            let mut pos = body_start;
            let dlen = match decode_chunked(&mut stream, &mut buf, body_start, &mut pos, &mut n, &mut decoded, body_deadline) {
                Ok(l) => l,
                Err(s) => {
                    let msg: &[u8] = if s == 413 { b"body too large\n" } else { b"bad request\n" };
                    send(&mut stream, s, "text/plain", msg, false, true);
                    return;
                }
            };
            consumed = pos;
            body = &decoded[..dlen];
        } else {
            let content_length = {
                let head = &buf[..head_end + 4];
                match content_length_of(head) {
                    Ok(c) => c,
                    Err(s) => {
                        send(&mut stream, s, "text/plain", b"bad request\n", false, true);
                        return;
                    }
                }
            };
            if content_length > BODY_CAP {
                send(&mut stream, 413, "text/plain", b"body too large\n", false, true);
                return;
            }
            if expect == Expect::Continue && content_length > 0 && !send_100(&mut stream) {
                return;
            }
            while n - body_start < content_length {
                if n == buf.len() {
                    send(&mut stream, 413, "text/plain", b"body too large\n", false, true);
                    return;
                }
                match read_before(&mut stream, &mut buf[n..], body_deadline) {
                    Ok(0) => return,
                    Ok(k) => n += k,
                    Err(_) => return,
                }
            }
            consumed = body_start + content_length;
            body = &buf[body_start..consumed];
        }
        // 3. parse (borrows buf), route, respond
        let head = &buf[..head_end + 4];
        let mut req = match parse_head(head) {
            Parse::NeedMore | Parse::Fail(400) => {
                send(&mut stream, 400, "text/plain", b"bad request\n", false, true);
                return;
            }
            Parse::Fail(s) => {
                send(&mut stream, s, "text/plain", b"bad request\n", false, true);
                return;
            }
            Parse::Ready(r) => r,
        };
        req.body = body;
        served += 1;
        // The last request a connection may serve is answered
        // `Connection: close`, even if the client asked to keep alive.
        let ka = keep_alive(&req) && served < KEEPALIVE_REQUESTS;
        REQUEST_COUNT.fetch_add(1, Ordering::Relaxed);
        if req.method == "OPTIONS" {
            if !send_options(&mut stream, &req, ka) {
                return;
            }
        } else if req.method == "GET" && path_of(req.target) == "/ws" {
            ws_serve(&mut stream, &req);
            return;
        } else if (req.method == "GET" || req.method == "HEAD") && path_of(req.target) == "/bytes" {
            if !send_ranges(&mut stream, &req, ka) {
                return;
            }
        } else if req.method == "GET" && path_of(req.target) == "/chunked" {
            if !send_chunked(&mut stream, ka) {
                return;
            }
        } else if (req.method == "GET" || req.method == "HEAD") && path_of(req.target) == "/events" {
            if !send_events(&mut stream, &req, ka) {
                return;
            }
        } else {
            let mut bbuf = [0u8; RESP_BODY_CAP];
            let mut out = Out::new(&mut bbuf);
            let (status, content_type) = route(&req, &mut out);
            let with_body = req.method != "HEAD" && !out.overflow;
            if !send(&mut stream, status, content_type, out.as_slice(), ka, with_body) {
                return;
            }
        }
        // 4. keep-alive: shift any pipelined bytes to the front and loop
        buf.copy_within(consumed..n, 0);
        n -= consumed;
        if !ka {
            return;
        }
    }
}

fn run() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:18080")?;
    eprintln!("zahttp on 127.0.0.1:18080 - std only, zero heap in the request path");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if ACTIVE_CONNS.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
                    // At capacity: a bare 503 with no request read, then close.
                    let mut s = s;
                    let _ = std::io::Write::write_all(
                        &mut s,
                        b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                    );
                } else {
                    thread::spawn(move || {
                        serve(s);
                        ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }
            Err(e) => eprintln!("accept: {}", e),
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("zahttp: {}", e);
    }
}
