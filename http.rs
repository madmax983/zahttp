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

// Read before the full parse: how many body bytes to expect. The borrow
// ends when this returns, so the body read can take &mut buf afterwards.
pub(crate) fn content_length_of(head: &[u8]) -> Result<usize, u16> {
    let mut rest = match head.windows(2).position(|w| w == b"\r\n") {
        Some(p) => &head[p + 2..],
        None => return Err(400),
    };
    while !rest.is_empty() {
        let eol = match rest.windows(2).position(|w| w == b"\r\n") {
            Some(p) => p,
            None => return Err(400),
        };
        let line = &rest[..eol];
        rest = &rest[eol + 2..];
        let colon = match line.iter().position(|&c| c == b':') {
            Some(p) => p,
            None => continue, // the full parser will reject this line later
        };
        if trim(&line[..colon]).eq_ignore_ascii_case(b"content-length") {
            let v = match core::str::from_utf8(trim(&line[colon + 1..])) {
                Ok(s) => s,
                Err(_) => return Err(400),
            };
            return match parse_usize(v) {
                Some(c) => Ok(c),
                None => Err(400),
            };
        }
    }
    Ok(0)
}

// path without the query string; borrows the target
pub(crate) fn path_of(target: &str) -> &str {
    match target.find('?') {
        Some(q) => &target[..q],
        None => target,
    }
}

// true when any Transfer-Encoding header lists the "chunked" token.
// Comma-separated values are honored; matching is case-insensitive.
pub(crate) fn is_chunked(head: &[u8]) -> bool {
    let mut rest = match head.windows(2).position(|w| w == b"\r\n") {
        Some(p) => &head[p + 2..],
        None => return false,
    };
    while !rest.is_empty() {
        let (line, next) = match rest.windows(2).position(|w| w == b"\r\n") {
            Some(p) => (&rest[..p], &rest[p + 2..]),
            None => (rest, &rest[rest.len()..]),
        };
        rest = next;
        let colon = match line.iter().position(|&c| c == b':') {
            Some(p) => p,
            None => continue,
        };
        if !trim(&line[..colon]).eq_ignore_ascii_case(b"transfer-encoding") {
            continue;
        }
        let mut tok = trim(&line[colon + 1..]);
        while !tok.is_empty() {
            let (t, next_tok) = match tok.iter().position(|&c| c == b',') {
                Some(p) => (&tok[..p], &tok[p + 1..]),
                None => (tok, &tok[tok.len()..]),
            };
            if trim(t).eq_ignore_ascii_case(b"chunked") {
                return true;
            }
            tok = next_tok;
        }
    }
    false
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

// Decode a chunked request body. Encoded bytes are pulled from the stream
// into buf (cursor *pos, valid bytes *n, compaction floor `floor`);
// decoded bytes accumulate in `out`. Returns the decoded length; *pos ends
// just past the body's final CRLF so keep-alive stays exact.
pub(crate) fn decode_chunked(
    stream: &mut TcpStream,
    buf: &mut [u8],
    floor: usize,
    pos: &mut usize,
    n: &mut usize,
    out: &mut [u8],
    deadline: Instant,
) -> Result<usize, u16> {
    let mut dlen = 0usize;
    let mut line = [0u8; 64];
    loop {
        let llen = read_line(stream, buf, floor, pos, n, &mut line, deadline)?;
        let size = parse_chunk_size(&line[..llen])?;
        if size == 0 {
            // final chunk: swallow trailers until the empty line
            loop {
                let tl = read_line(stream, buf, floor, pos, n, &mut line, deadline)?;
                if tl == 0 {
                    break;
                }
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

// Scan the raw head for an Expect header. HTTP/1.0 clients never get a
// 100-continue: the interim status would confuse their framing.
pub(crate) fn expect_of(head: &[u8]) -> Expect {
    let req_line_end = match head.windows(2).position(|w| w == b"\r\n") {
        Some(p) => p,
        None => return Expect::None,
    };
    if head[..req_line_end].ends_with(b"HTTP/1.0") {
        return Expect::None;
    }
    let mut rest = &head[req_line_end + 2..];
    while !rest.is_empty() {
        let eol = match rest.windows(2).position(|w| w == b"\r\n") {
            Some(p) => p,
            None => return Expect::None,
        };
        let line = &rest[..eol];
        rest = &rest[eol + 2..];
        let colon = match line.iter().position(|&c| c == b':') {
            Some(p) => p,
            None => continue,
        };
        if trim(&line[..colon]).eq_ignore_ascii_case(b"expect") {
            let v = trim(&line[colon + 1..]);
            if v.eq_ignore_ascii_case(b"100-continue") {
                return Expect::Continue;
            }
            return Expect::Other;
        }
    }
    Expect::None
}

pub(crate) fn send_100(stream: &mut TcpStream, deadline: Instant) -> bool {
    write_all_before(stream, b"HTTP/1.1 100 Continue\r\n\r\n", deadline)
}
