// zahttp module: sse (the infinite feed) — zero deps, zero heap. See main.rs for the rules.

// ---- Server-Sent Events (HTML spec): GET /events -----------------------
// An infinite live feed, paced by a 1s timer. `Content-Type:
// text/event-stream`, `Cache-Control: no-cache`, chunked on HTTP/1.1,
// close-delimited on HTTP/1.0. `Last-Event-ID` resumes the tick counter
// after that id (garbage or absent means 0). Every frame is built in one
// reused 128-byte stack buffer — no allocator anywhere. The connection
// thread parks in thread::sleep between ticks; the first failed write
// (client gone) ends the stream.

use std::io::Write;
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use crate::buf::{date_now, parse_u64b, push_hex_usize, Out};
use crate::http::{header, Request};

pub(crate) const SSE_INTERVAL: Duration = Duration::from_secs(1);
const SSE_PREAMBLE: &[u8] = b": zahttp event stream\nretry: 3000\n\n";

pub(crate) fn sse_tick(id: u64, o: &mut Out) {
    o.push_str("id: ");
    o.push_u64(id);
    o.push_str("\nevent: tick\ndata: ");
    o.push_u64(id);
    o.push_str("\n\n");
}

fn write_sse_chunk(stream: &mut TcpStream, payload: &[u8]) -> bool {
    let mut cbuf = [0u8; 24];
    let mut c = Out::new(&mut cbuf);
    push_hex_usize(&mut c, payload.len());
    c.push_str("\r\n");
    if c.overflow {
        return false;
    }
    stream.write_all(c.as_slice()).is_ok()
        && stream.write_all(payload).is_ok()
        && stream.write_all(b"\r\n").is_ok()
}

pub(crate) fn send_events(stream: &mut TcpStream, req: &Request, keep_alive: bool) -> bool {
    let with_body = req.method == "GET"; // HEAD: headers only
    let http11 = req.version == 1;
    // HTTP/1.0 is close-delimited: no Content-Length, no chunked framing,
    // so the connection must close even if the client asked for keep-alive.
    // (On 1.1 the stream is infinite by design, so keep-alive is moot: the
    // connection belongs to the feed until the client leaves.)
    let alive = http11 && keep_alive;
    let mut hbuf = [0u8; 512];
    let mut h = Out::new(&mut hbuf);
    let mut date = [0u8; 29];
    date_now(&mut date);
    h.push_str("HTTP/1.1 200 OK\r\nDate: ");
    h.push(&date);
    h.push_str("\r\nServer: zahttp/0.1\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n");
    if with_body && http11 {
        // HEAD carries no body, so it must not promise chunked framing.
        h.push_str("Transfer-Encoding: chunked\r\n");
    }
    h.push_str("Connection: ");
    h.push_str(if alive { "keep-alive" } else { "close" });
    h.push_str("\r\n\r\n");
    if h.overflow || !stream.write_all(h.as_slice()).is_ok() {
        return false;
    }
    if !with_body {
        return alive;
    }
    let start = match header(req, "last-event-id") {
        Some(v) => match parse_u64b(v.trim().as_bytes()) {
            Some(n) => n,
            None => 0,
        },
        None => 0,
    };
    let mut fbuf = [0u8; 128];
    // one frame at a time: chunked on 1.1, raw on 1.0
    let mut emit = |payload: &[u8]| -> bool {
        if http11 {
            write_sse_chunk(stream, payload)
        } else {
            stream.write_all(payload).is_ok()
        }
    };
    if !emit(SSE_PREAMBLE) {
        return false;
    }
    // The feed never ends: tick forever, one per SSE_INTERVAL. A failed
    // write means the client is gone; that is the only exit.
    let mut id = start.wrapping_add(1);
    if id == 0 {
        id = 1; // id 0 is reserved for "no resume point"
    }
    loop {
        let mut f = Out::new(&mut fbuf);
        sse_tick(id, &mut f);
        if f.overflow {
            return false;
        }
        if !emit(f.as_slice()) {
            return false;
        }
        id = id.wrapping_add(1);
        if id == 0 {
            id = 1;
        }
        thread::sleep(SSE_INTERVAL);
    }
}
