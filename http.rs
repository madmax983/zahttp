// zahttp module: http (request parsing + framing) — zero deps, zero heap. See main.rs for the rules.

use std::io::Read;
use std::net::TcpStream;
use std::time::Instant;

use crate::buf::{write_all_before, HDR_MAX};

// ---- request parsing: everything borrows from the read buffer ----------

#[derive(Clone, Copy)]
pub(crate) struct Header<'a> {
    pub(crate) name: &'a str,
    pub(crate) value: &'a str,
}

pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) target: &'a str,
    pub(crate) version: u8, // 0 or 1
    pub(crate) headers: [Option<Header<'a>>; HDR_MAX],
    pub(crate) header_count: usize,
    pub(crate) body: &'a [u8],
    pub(crate) trailers: TrailerStore, // chunked request trailers, if any
}

pub(crate) enum Parse<'a> {
    NeedMore,
    Ready(Request<'a>),
    Fail(u16),
}

pub(crate) fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0;
    let mut e = b.len();
    while s < e && (b[s] == b' ' || b[s] == b'\t') {
        s += 1;
    }
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t') {
        e -= 1;
    }
    &b[s..e]
}

pub(crate) fn split_request_line(line: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let a = line.iter().position(|&c| c == b' ')?;
    let rest = &line[a + 1..];
    let b = rest.iter().position(|&c| c == b' ')?;
    let method = &line[..a];
    let target = &rest[..b];
    let version = &rest[b + 1..];
    if method.is_empty() || target.is_empty() || version.contains(&b' ') {
        return None;
    }
    Some((method, target, version))
}

pub(crate) fn parse_head(head: &[u8]) -> Parse<'_> {
    let eol = match head.windows(2).position(|w| w == b"\r\n") {
        Some(p) => p,
        None => return Parse::NeedMore, // unreachable: caller found \r\n\r\n
    };
    let (method, target, version) = match split_request_line(&head[..eol]) {
        Some(t) => t,
        None => return Parse::Fail(400),
    };
    let version = match version {
        b"HTTP/1.1" => 1u8,
        b"HTTP/1.0" => 0u8,
        _ => return Parse::Fail(505),
    };
    let method = match core::str::from_utf8(method) {
        Ok(s) if !s.is_empty() => s,
        _ => return Parse::Fail(400),
    };
    let target = match core::str::from_utf8(target) {
        // "*" is the asterisk-form target, only meaningful for OPTIONS
        // (RFC 7230 5.3.4); anything else must be an origin-form path.
        Ok(s) if s.starts_with('/') || s == "*" => s,
        _ => return Parse::Fail(400),
    };
    let mut headers: [Option<Header>; HDR_MAX] = [None; HDR_MAX];
    let mut count = 0usize;
    let mut rest = &head[eol + 2..];
    while !rest.is_empty() {
        let eol = match rest.windows(2).position(|w| w == b"\r\n") {
            Some(p) => p,
            None => return Parse::Fail(400),
        };
        let line = &rest[..eol];
        rest = &rest[eol + 2..];
        if line.is_empty() {
            continue; // defensive; the head ends before the blank line
        }
        let colon = match line.iter().position(|&c| c == b':') {
            Some(p) => p,
            None => return Parse::Fail(400),
        };
        let name = match core::str::from_utf8(trim(&line[..colon])) {
            Ok(s) if !s.is_empty() => s,
            _ => return Parse::Fail(400),
        };
        let value = match core::str::from_utf8(trim(&line[colon + 1..])) {
            Ok(s) => s,
            _ => return Parse::Fail(400),
        };
        if count == HDR_MAX {
            return Parse::Fail(431);
        }
        headers[count] = Some(Header { name, value });
        count += 1;
    }
    Parse::Ready(Request {
        method,
        target,
        version,
        headers,
        header_count: count,
        body: &[],
        trailers: TrailerStore::empty(),
    })
}

pub(crate) fn header<'a>(req: &'a Request<'a>, name: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < req.header_count {
        if let Some(h) = req.headers[i] {
            if h.name.eq_ignore_ascii_case(name) {
                return Some(h.value);
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn parse_usize(s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut v = 0usize;
    for c in s.bytes() {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as usize)?;
    }
    Some(v)
}

// path without the query string; borrows the target
pub(crate) fn path_of(target: &str) -> &str {
    match target.find('?') {
        Some(q) => &target[..q],
        None => target,
    }
}

// Combined result of scan_head: everything serve() needs to know about
// framing and expectations before it reads a single body byte.
pub(crate) struct HeadScan {
    pub(crate) chunked: bool,
    pub(crate) content_length: Result<usize, u16>,
    pub(crate) expect: Expect,
}

// One pass over the header lines that answers what is_chunked,
// content_length_of, and expect_of used to answer with three separate
// full walks of the same bytes: is any Transfer-Encoding "chunked"-listed
// (comma-separated, case-insensitive, first true occurrence across
// possibly-repeated headers), what does the first Content-Length say, and
// what does the first Expect say (HTTP/1.0 clients never get one). Each
// field keeps the exact matching, trimming, and error rules of its
// original function; only the header-line walk itself is shared.
pub(crate) fn scan_head(head: &[u8]) -> HeadScan {
    let req_line_end = match head.windows(2).position(|w| w == b"\r\n") {
        Some(p) => p,
        None => return HeadScan { chunked: false, content_length: Err(400), expect: Expect::None },
    };
    let http10 = head[..req_line_end].ends_with(b"HTTP/1.0");

    let mut chunked = false;
    let mut content_length: Result<usize, u16> = Ok(0);
    let mut content_length_found = false;
    let mut expect = Expect::None;
    let mut expect_found = false;

    let mut rest = &head[req_line_end + 2..];
    while !rest.is_empty() {
        let (line, next) = match rest.windows(2).position(|w| w == b"\r\n") {
            Some(p) => (&rest[..p], &rest[p + 2..]),
            None => (rest, &rest[rest.len()..]),
        };
        rest = next;
        let colon = match line.iter().position(|&c| c == b':') {
            Some(p) => p,
            None => continue, // the full parser will reject this line later
        };
        let name = trim(&line[..colon]);
        let value = &line[colon + 1..];

        if !chunked && name.eq_ignore_ascii_case(b"transfer-encoding") {
            let mut tok = trim(value);
            while !tok.is_empty() {
                let (t, next_tok) = match tok.iter().position(|&c| c == b',') {
                    Some(p) => (&tok[..p], &tok[p + 1..]),
                    None => (tok, &tok[tok.len()..]),
                };
                if trim(t).eq_ignore_ascii_case(b"chunked") {
                    chunked = true;
                    break;
                }
                tok = next_tok;
            }
        } else if !content_length_found && name.eq_ignore_ascii_case(b"content-length") {
            content_length_found = true;
            content_length = match core::str::from_utf8(trim(value)) {
                Ok(s) => match parse_usize(s) {
                    Some(c) => Ok(c),
                    None => Err(400),
                },
                Err(_) => Err(400),
            };
        } else if !expect_found && !http10 && name.eq_ignore_ascii_case(b"expect") {
            expect_found = true;
            let v = trim(value);
            expect = if v.eq_ignore_ascii_case(b"100-continue") { Expect::Continue } else { Expect::Other };
        }
    }
    HeadScan { chunked, content_length, expect }
}

// Parse a chunk-size line: 1*HEXDIG, optional ";extension", tolerant of
// surrounding whitespace. 400 on garbage, 413 on overflow.
pub(crate) fn parse_chunk_size(line: &[u8]) -> Result<usize, u16> {
    let mut end = line.len();
    if let Some(semi) = line.iter().position(|&c| c == b';') {
        end = semi;
    }
    while end > 0 && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
        end -= 1;
    }
    let mut start = 0;
    while start < end && (line[start] == b' ' || line[start] == b'\t') {
        start += 1;
    }
    if start == end {
        return Err(400);
    }
    let mut v = 0usize;
    let mut i = start;
    while i < end {
        let d = match line[i] {
            b'0'..=b'9' => (line[i] - b'0') as usize,
            b'a'..=b'f' => (line[i] - b'a' + 10) as usize,
            b'A'..=b'F' => (line[i] - b'A' + 10) as usize,
            _ => return Err(400),
        };
        v = match v.checked_mul(16).and_then(|x| x.checked_add(d)) {
            Some(x) => x,
            None => return Err(413),
        };
        i += 1;
    }
    Ok(v)
}

// Read one CRLF-terminated line from the stream into `line` (without the
// CRLF). `buf[*pos..*n]` holds unconsumed bytes; `floor` is the lowest
// index compaction may use, so the request head stays intact.
// ---- deadline-bounded reads: BEAT THE DRIBBLE ---------------------------
// Every serve() read goes through here. The socket timeout is re-armed to
// the time left before each read, so a 1-byte-per-4.9s slowloris dies at
// the total deadline instead of lingering forever. On expiry returns a
// TimedOut error without reading; the caller closes the connection.
pub(crate) fn read_before(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<usize> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
    }
    let _ = stream.set_read_timeout(Some(remaining));
    stream.read(buf)
}

pub(crate) fn read_line(
    stream: &mut TcpStream,
    buf: &mut [u8],
    floor: usize,
    pos: &mut usize,
    n: &mut usize,
    line: &mut [u8],
    deadline: Instant,
) -> Result<usize, u16> {
    let mut llen = 0usize;
    loop {
        if let Some(rel) = buf[*pos..*n].windows(2).position(|w| w == b"\r\n") {
            if rel > line.len() - llen {
                return Err(400);
            }
            line[llen..llen + rel].copy_from_slice(&buf[*pos..*pos + rel]);
            *pos += rel + 2;
            return Ok(llen + rel);
        }
        let avail = *n - *pos;
        if avail > 0 {
            if llen + avail > line.len() {
                return Err(400);
            }
            line[llen..llen + avail].copy_from_slice(&buf[*pos..*n]);
            llen += avail;
            *pos = *n;
        }
        if *n == buf.len() {
            buf.copy_within(*pos..*n, floor);
            *n = floor + (*n - *pos);
            *pos = floor;
        }
        match read_before(stream, &mut buf[*n..], deadline) {
            Ok(0) => return Err(400),
            Ok(k) => *n += k,
            Err(_) => return Err(400),
        }
    }
}

// Consume exactly one CRLF from the stream. Ok(false) means the bytes
// were present but were not a CRLF.
pub(crate) fn expect_crlf(
    stream: &mut TcpStream,
    buf: &mut [u8],
    floor: usize,
    pos: &mut usize,
    n: &mut usize,
    deadline: Instant,
) -> Result<bool, u16> {
    while *n - *pos < 2 {
        if *n == buf.len() {
            buf.copy_within(*pos..*n, floor);
            *n = floor + (*n - *pos);
            *pos = floor;
        }
        match read_before(stream, &mut buf[*n..], deadline) {
            Ok(0) => return Err(400),
            Ok(k) => *n += k,
            Err(_) => return Err(400),
        }
    }
    if buf[*pos] == b'\r' && buf[*pos + 1] == b'\n' {
        *pos += 2;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---- chunked trailers (RFC 9112 7.1.2) ---------------------------------

pub(crate) const MAX_TRAILERS: usize = 16; // most trailer lines per request
pub(crate) const TRAILER_LINE_MAX: usize = 256; // longest single trailer line

// Accepted trailers, owned copies on the stack: trailer bytes are read
// out of the shared stream buffer, whose contents shift under keep-alive
// compaction, so borrowing them would dangle.
#[derive(Clone, Copy)]
pub(crate) struct TrailerStore {
    pub(crate) count: usize,
    pub(crate) lines: [[u8; TRAILER_LINE_MAX]; MAX_TRAILERS],
    pub(crate) lens: [usize; MAX_TRAILERS],
}

impl TrailerStore {
    pub(crate) fn empty() -> TrailerStore {
        TrailerStore {
            count: 0,
            lines: [[0u8; TRAILER_LINE_MAX]; MAX_TRAILERS],
            lens: [0usize; MAX_TRAILERS],
        }
    }
}

fn is_token(name: &[u8]) -> bool {
    const TCHARS: &[u8] = b"!#$%&'*+-.^_`|~";
    !name.is_empty() && name.iter().all(|&c| c.is_ascii_alphanumeric() || TCHARS.contains(&c))
}

// Validate one trailer line; returns the trimmed field name so the caller
// can run the forbidden-field screen.
fn trailer_name(line: &[u8]) -> Result<&[u8], u16> {
    let colon = match line.iter().position(|&c| c == b':') {
        Some(p) => p,
        None => return Err(400),
    };
    let name = trim(&line[..colon]);
    if !is_token(name) {
        return Err(400);
    }
    Ok(name)
}

// Fields a sender must never put in a trailer section (RFC 9112 7.1.2):
// framing, routing, auth, expectations, and content-processing fields.
// Content-Length / Transfer-Encoding here are the request-smuggling set,
// so they fail the whole request with 400 instead of being ignored.
fn is_forbidden_trailer(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"transfer-encoding")
        || name.eq_ignore_ascii_case(b"content-length")
        || name.eq_ignore_ascii_case(b"trailer")
        || name.eq_ignore_ascii_case(b"te")
        || name.eq_ignore_ascii_case(b"host")
        || name.eq_ignore_ascii_case(b"authorization")
        || name.eq_ignore_ascii_case(b"proxy-authenticate")
        || name.eq_ignore_ascii_case(b"proxy-authorization")
        || name.eq_ignore_ascii_case(b"expect")
        || name.eq_ignore_ascii_case(b"content-encoding")
        || name.eq_ignore_ascii_case(b"content-type")
        || name.eq_ignore_ascii_case(b"content-range")
}

// Decode a chunked request body. Encoded bytes are pulled from the stream
// into buf (cursor *pos, valid bytes *n, compaction floor `floor`);
// decoded bytes accumulate in `out`. Accepted trailer lines accumulate in
// `trailers`. Returns the decoded length; *pos ends just past the body's
// final CRLF so keep-alive stays exact.
pub(crate) fn decode_chunked(
    stream: &mut TcpStream,
    buf: &mut [u8],
    floor: usize,
    pos: &mut usize,
    n: &mut usize,
    out: &mut [u8],
    trailers: &mut TrailerStore,
    deadline: Instant,
) -> Result<usize, u16> {
    let mut dlen = 0usize;
    let mut line = [0u8; 64];
    loop {
        let llen = read_line(stream, buf, floor, pos, n, &mut line, deadline)?;
        let size = parse_chunk_size(&line[..llen])?;
        if size == 0 {
            // final chunk: parse the trailer section into stack-owned
            // storage. Each line is syntax-checked (`name: value` with a
            // token-only name), bounded (at most MAX_TRAILERS lines, each
            // at most TRAILER_LINE_MAX bytes), and screened against the
            // RFC 9112 7.1.2 forbidden list (framing, routing, auth, and
            // content fields — the request-smuggling set). Any violation
            // is a 400; accepted trailers are kept for the /trailers
            // route. Unannounced trailers are accepted: no `Trailer:`
            // header is required.
            loop {
                // At the bound only the empty terminator may follow; a
                // 17th real trailer line is a 400.
                if trailers.count == MAX_TRAILERS {
                    let tl = read_line(stream, buf, floor, pos, n, &mut line, deadline)?;
                    if tl != 0 {
                        return Err(400);
                    }
                    break;
                }
                let slot = &mut trailers.lines[trailers.count];
                let tl = read_line(stream, buf, floor, pos, n, slot, deadline)?;
                if tl == 0 {
                    break;
                }
                let name = trailer_name(&slot[..tl])?;
                if is_forbidden_trailer(name) {
                    return Err(400);
                }
                trailers.lens[trailers.count] = tl;
                trailers.count += 1;
            }
            return Ok(dlen);
        }
        if size > out.len() - dlen {
            return Err(413);
        }
        let mut remaining = size;
        while remaining > 0 {
            if *pos == *n {
                if *n == buf.len() {
                    buf.copy_within(*pos..*n, floor);
                    *n = floor;
                    *pos = floor;
                }
                match read_before(stream, &mut buf[*n..], deadline) {
                    Ok(0) => return Err(400),
                    Ok(k) => *n += k,
                    Err(_) => return Err(400),
                }
            }
            let avail = (*n - *pos).min(remaining);
            out[dlen..dlen + avail].copy_from_slice(&buf[*pos..*pos + avail]);
            *pos += avail;
            dlen += avail;
            remaining -= avail;
        }
        if !expect_crlf(stream, buf, floor, pos, n, deadline)? {
            return Err(400);
        }
    }
}

pub(crate) fn keep_alive(req: &Request) -> bool {
    let conn = header(req, "connection");
    let wants_close = matches!(conn, Some(v) if v.eq_ignore_ascii_case("close"));
    let wants_keep = matches!(conn, Some(v) if v.eq_ignore_ascii_case("keep-alive"));
    match req.version {
        1 => !wants_close,
        0 => wants_keep,
        _ => false,
    }
}

pub(crate) fn find_double_crlf(hay: &[u8]) -> Option<usize> {
    hay.windows(4).position(|w| w == b"\r\n\r\n")
}

#[derive(PartialEq, Eq)]
pub(crate) enum Expect {
    None,
    Continue,
    Other,
}

pub(crate) fn send_100(stream: &mut TcpStream, deadline: Instant) -> bool {
    write_all_before(stream, b"HTTP/1.1 100 Continue\r\n\r\n", deadline)
}
