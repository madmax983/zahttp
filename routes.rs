// zahttp module: routes (dispatch + small handlers) — zero deps, zero heap. See main.rs for the rules.

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::Ordering;

use crate::alloc::{ALLOC_COUNT, REQUEST_COUNT};
use crate::buf::{date_now, push_hex_u64, push_hex_usize, write2, write3, Out, RESP_HEAD_CAP};
use crate::http::{header, path_of, Request};
use crate::multipart::serve_upload;

// ---- routing ------------------------------------------------------------

pub(crate) const INDEX: &str = "<!doctype html><html><head><title>zahttp</title><style>body{background:#0d0d0f;color:#c9a0ff;font-family:monospace;max-width:640px;margin:4rem auto;padding:0 1rem}h1{font-size:3rem}a{color:#7df9ff}</style></head><body><h1>zahttp &#x1f921;</h1><p>zero-dependency, zero-allocation HTTP/1.1. every byte on the stack.</p><ul><li><a href=\"/health\">/health</a></li><li><a href=\"/time\">/time</a></li><li><a href=\"/headers\">/headers</a></li><li><a href=\"/metrics\">/metrics</a></li><li><a href=\"/allocs\">/allocs</a></li><li><a href=\"/chunked\">/chunked</a> (chunked stream)</li><li><code>/ws</code> (websocket)</li><li><a href=\"/bytes\">/bytes</a> (ranges, conditionals, gzip)</li><li><code>/upload</code> (multipart/form-data)</li><li><a href=\"/events\">/events</a> (server-sent events)</li></ul><p>POST a body to <code>/echo</code> and get it back. Chunked request bodies welcome.</p></body></html>";

// ---- OPTIONS (RFC 7231 4.2.7) -------------------------------------------
// The methods each resource actually speaks; OPTIONS reports them in
// Allow. `*` asks about the server as a whole.
pub(crate) fn allow_for(path: &str) -> Option<&'static str> {
    Some(match path {
        "*" => "GET, HEAD, POST, OPTIONS",
        "/" | "/health" => "GET, HEAD, OPTIONS",
        "/bytes" => "GET, HEAD, OPTIONS",
        "/metrics" | "/allocs" | "/time" | "/headers" => "GET, OPTIONS",
        "/echo" | "/upload" => "POST, OPTIONS",
        "/chunked" | "/ws" => "GET, OPTIONS",
        "/events" => "GET, HEAD, OPTIONS",
        _ => return None,
    })
}

pub(crate) fn send_options(stream: &mut TcpStream, req: &Request, keep_alive: bool) -> bool {
    let allow = match allow_for(path_of(req.target)) {
        Some(a) => a,
        None => return send(stream, 404, "text/plain", b"not found\n", keep_alive, true),
    };
    let mut hbuf = [0u8; 512];
    let mut h = Out::new(&mut hbuf);
    let mut date = [0u8; 29];
    date_now(&mut date);
    h.push_str("HTTP/1.1 200 OK\r\nDate: ");
    h.push(&date);
    h.push_str("\r\nServer: zahttp/0.1\r\nAllow: ");
    h.push_str(allow);
    h.push_str("\r\n");
    // A CORS preflight (Origin + Access-Control-Request-Method) gets the
    // CORS answer headers too; that is what OPTIONS is for in practice.
    if header(req, "origin").is_some() && header(req, "access-control-request-method").is_some() {
        h.push_str("Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: ");
        h.push_str(allow);
        h.push_str("\r\nAccess-Control-Max-Age: 86400\r\n");
    }
    h.push_str("Content-Length: 0\r\nConnection: ");
    h.push_str(if keep_alive { "keep-alive" } else { "close" });
    h.push_str("\r\n\r\n");
    !h.overflow && stream.write_all(h.as_slice()).is_ok()
}

pub(crate) fn route(req: &Request, out: &mut Out) -> (u16, &'static str) {
    let path = path_of(req.target);
    let known = path == "/"
        || path == "/health"
        || path == "/metrics"
        || path == "/allocs"
        || path == "/time"
        || path == "/headers"
        || path == "/echo"
        || path == "/chunked"
        || path == "/ws"
        || path == "/bytes"
        || path == "/upload"
        || path == "/events";
    match (req.method, path) {
        ("GET", "/") | ("HEAD", "/") => {
            out.push_str(INDEX);
            (200, "text/html")
        }
        ("GET", "/health") | ("HEAD", "/health") => {
            out.push_str("ok\n");
            (200, "text/plain")
        }
        ("GET", "/metrics") => {
            out.push_str("http_requests_total ");
            out.push_u64(REQUEST_COUNT.load(Ordering::Relaxed));
            out.push_str("\n");
            (200, "text/plain")
        }
        ("GET", "/allocs") => {
            out.push_str("heap_allocations_total ");
            out.push_u64(ALLOC_COUNT.load(Ordering::Relaxed));
            out.push_str("\n");
            (200, "text/plain")
        }
        ("GET", "/time") => {
            let mut d = [0u8; 29];
            date_now(&mut d);
            out.push(&d);
            out.push_str("\n");
            (200, "text/plain")
        }
        ("GET", "/headers") => {
            let mut i = 0;
            while i < req.header_count {
                if let Some(h) = req.headers[i] {
                    out.push_str(h.name);
                    out.push_str(": ");
                    out.push_str(h.value);
                    out.push_str("\n");
                }
                i += 1;
            }
            (200, "text/plain")
        }
        ("POST", "/echo") => {
            out.push(req.body);
            (200, "text/plain")
        }
        ("POST", "/upload") => match serve_upload(req, out) {
            Ok(()) => (200, "text/plain"),
            Err(status) => (status, "text/plain"),
        },
        _ if known => {
            out.push_str("method not allowed\n");
            (405, "text/plain")
        }
        _ => {
            out.push_str("not found\n");
            (404, "text/plain")
        }
    }
}

// ---- response -----------------------------------------------------------

pub(crate) fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

pub(crate) fn send(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    keep_alive: bool,
    with_body: bool,
) -> bool {
    let mut hbuf = [0u8; RESP_HEAD_CAP];
    let mut h = Out::new(&mut hbuf);
    let mut date = [0u8; 29];
    date_now(&mut date);
    h.push_str("HTTP/1.1 ");
    h.push_u64(status as u64);
    h.push_str(" ");
    h.push_str(reason(status));
    h.push_str("\r\nDate: ");
    h.push(&date);
    h.push_str("\r\nServer: zahttp/0.1\r\nContent-Type: ");
    h.push_str(content_type);
    h.push_str("\r\nContent-Length: ");
    h.push_u64(body.len() as u64);
    h.push_str("\r\nConnection: ");
    h.push_str(if keep_alive { "keep-alive" } else { "close" });
    h.push_str("\r\n\r\n");
    if h.overflow {
        return false;
    }
    let payload: &[u8] = if with_body { body } else { &[] };
    // One writev for head+body: keeps the tiny body from stalling
    // behind the header's delayed ACK (see buf::write2).
    write2(stream, h.as_slice(), payload)
}

// Stream a generated body with Transfer-Encoding: chunked. 64 chunks of
// deterministic LCG hex, each framed as <hexlen>\r\n<payload>\r\n,
// terminated by 0\r\n\r\n. No Content-Length, all fixed buffers.
pub(crate) fn send_chunked(stream: &mut TcpStream, keep_alive: bool) -> bool {
    let mut hbuf = [0u8; RESP_HEAD_CAP];
    let mut h = Out::new(&mut hbuf);
    let mut date = [0u8; 29];
    date_now(&mut date);
    h.push_str("HTTP/1.1 200 OK\r\nDate: ");
    h.push(&date);
    h.push_str("\r\nServer: zahttp/0.1\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: ");
    h.push_str(if keep_alive { "keep-alive" } else { "close" });
    h.push_str("\r\n\r\n");
    if h.overflow || !stream.write_all(h.as_slice()).is_ok() {
        return false;
    }
    let mut rng = 0x12345678u64;
    let mut i = 0u32;
    while i < 64 {
        let mut pbuf = [0u8; 96];
        let mut p = Out::new(&mut pbuf);
        p.push_str("chunk ");
        p.push_u64(i as u64);
        p.push_str(": ");
        let mut k = 0;
        while k < 4 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            push_hex_u64(&mut p, rng);
            if k < 3 {
                p.push_str(" ");
            }
            k += 1;
        }
        p.push_str("\n");
        if p.overflow {
            return false;
        }
        let payload = p.as_slice();
        let mut cbuf = [0u8; 24];
        let mut c = Out::new(&mut cbuf);
        push_hex_usize(&mut c, payload.len());
        c.push_str("\r\n");
        if c.overflow {
            return false;
        }
        if !write3(stream, c.as_slice(), payload, b"\r\n") {
            return false;
        }
        i += 1;
    }
    stream.write_all(b"0\r\n\r\n").is_ok()
}
