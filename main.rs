// zahttp - a zero-dependency, zero-allocation HTTP/1.1 server.
//
// The rules of the game:
//   * std only. One file. No Cargo project, no crates, no extern items.
//   * No heap in our code, ever: every byte lives in a fixed-size stack
//     buffer, and parsing only ever borrows slices of that buffer.
//   * Errors become status codes, never panics: the hot loop has no
//     panicking helpers.
//   * A counting global allocator backs the "we allocate nothing" claim:
//     hit /allocs twice over one connection and watch the number not move.
//     (Spawning the per-connection thread is std's business and may bump
//     the counter; the per-request path is ours and stays flat.)

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

// ---- allocation counter: the proof -------------------------------------

struct Counting;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static REQUEST_COUNT: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ---- fixed capacities: the whole server lives in these ------------------

const READ_CAP: usize = 8192; // largest request head we will buffer
const BODY_CAP: usize = 4096; // largest request body we accept
const HDR_MAX: usize = 32; // most headers per request
const RESP_HEAD_CAP: usize = 512;
const RESP_BODY_CAP: usize = 4096;

// ---- Out: a tiny non-allocating byte writer over a fixed buffer ---------

struct Out<'b> {
    buf: &'b mut [u8],
    len: usize,
    overflow: bool,
}

impl<'b> Out<'b> {
    fn new(buf: &'b mut [u8]) -> Out<'b> {
        Out { buf, len: 0, overflow: false }
    }
    fn push(&mut self, bytes: &[u8]) {
        let space = self.buf.len().saturating_sub(self.len);
        let take = bytes.len().min(space);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        if take < bytes.len() {
            self.overflow = true;
        }
    }
    fn push_str(&mut self, s: &str) {
        self.push(s.as_bytes());
    }
    fn push_u64(&mut self, mut v: u64) {
        let mut tmp = [0u8; 20];
        let mut n = 0usize;
        if v == 0 {
            tmp[0] = b'0';
            n = 1;
        } else {
            while v > 0 && n < tmp.len() {
                tmp[n] = b'0' + (v % 10) as u8;
                v /= 10;
                n += 1;
            }
        }
        while n > 0 {
            n -= 1;
            self.push(&tmp[n..n + 1]);
        }
    }
    fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

fn push2(o: &mut Out, v: u64) {
    o.push(&[b'0' + (v / 10) as u8, b'0' + (v % 10) as u8]);
}

fn push4(o: &mut Out, v: u64) {
    o.push(&[
        b'0' + (v / 1000) as u8,
        b'0' + ((v / 100) % 10) as u8,
        b'0' + ((v / 10) % 10) as u8,
        b'0' + (v % 10) as u8,
    ]);
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn push_hex_u64(o: &mut Out, v: u64) {
    let mut tmp = [0u8; 16];
    let mut n = 0usize;
    if v == 0 {
        tmp[0] = b'0';
        n = 1;
    } else {
        let mut x = v;
        while x > 0 && n < tmp.len() {
            tmp[n] = HEX[(x & 15) as usize];
            x >>= 4;
            n += 1;
        }
    }
    while n > 0 {
        n -= 1;
        o.push(&tmp[n..n + 1]);
    }
}

fn push_hex_usize(o: &mut Out, v: usize) {
    push_hex_u64(o, v as u64);
}

// ---- SHA-1 (FIPS 180-4), hand-rolled ------------------------------------

fn sha1_block(block: &[u8], h: &mut [u32; 5]) {
    let mut w = [0u32; 80];
    let mut i = 0;
    while i < 16 {
        w[i] = ((block[4 * i] as u32) << 24)
            | ((block[4 * i + 1] as u32) << 16)
            | ((block[4 * i + 2] as u32) << 8)
            | (block[4 * i + 3] as u32);
        i += 1;
    }
    while i < 80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        i += 1;
    }
    let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
    i = 0;
    while i < 80 {
        let (f, k) = if i < 20 {
            ((b & c) | (!b & d), 0x5A827999u32)
        } else if i < 40 {
            (b ^ c ^ d, 0x6ED9EBA1)
        } else if i < 60 {
            ((b & c) | (b & d) | (c & d), 0x8F1BBCDC)
        } else {
            (b ^ c ^ d, 0xCA62C1D6)
        };
        let tmp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(w[i]);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = tmp;
        i += 1;
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
}

fn sha1(msg: &[u8], out: &mut [u8; 20]) {
    let mut h = [0x67452301u32, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut chunks = msg.chunks_exact(64);
    for block in &mut chunks {
        sha1_block(block, &mut h);
    }
    let rem = chunks.remainder();
    let rlen = rem.len();
    let mut pad = [0u8; 128];
    pad[..rlen].copy_from_slice(rem);
    pad[rlen] = 0x80;
    let total = ((rlen + 9 + 63) / 64) * 64; // rlen+1+8 rounded up; always <= 128
    let bitlen = (msg.len() as u64).wrapping_mul(8);
    let mut i = 0;
    while i < 8 {
        pad[total - 1 - i] = (bitlen >> (8 * i)) as u8;
        i += 1;
    }
    let mut off = 0;
    while off < total {
        sha1_block(&pad[off..off + 64], &mut h);
        off += 64;
    }
    i = 0;
    while i < 5 {
        out[4 * i] = (h[i] >> 24) as u8;
        out[4 * i + 1] = (h[i] >> 16) as u8;
        out[4 * i + 2] = (h[i] >> 8) as u8;
        out[4 * i + 3] = h[i] as u8;
        i += 1;
    }
}

// ---- base64 (RFC 4648), hand-rolled -------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

// Encodes into `out` (must fit ((len+2)/3)*4 bytes); returns encoded length.
fn base64_encode(input: &[u8], out: &mut [u8]) -> usize {
    let mut ip = 0usize;
    let mut op = 0usize;
    while ip + 3 <= input.len() {
        let n = ((input[ip] as u32) << 16) | ((input[ip + 1] as u32) << 8) | (input[ip + 2] as u32);
        out[op] = B64[((n >> 18) & 63) as usize];
        out[op + 1] = B64[((n >> 12) & 63) as usize];
        out[op + 2] = B64[((n >> 6) & 63) as usize];
        out[op + 3] = B64[(n & 63) as usize];
        ip += 3;
        op += 4;
    }
    let rem = input.len() - ip;
    if rem == 1 {
        let n = (input[ip] as u32) << 16;
        out[op] = B64[((n >> 18) & 63) as usize];
        out[op + 1] = B64[((n >> 12) & 63) as usize];
        out[op + 2] = b'=';
        out[op + 3] = b'=';
        op += 4;
    } else if rem == 2 {
        let n = ((input[ip] as u32) << 16) | ((input[ip + 1] as u32) << 8);
        out[op] = B64[((n >> 18) & 63) as usize];
        out[op + 1] = B64[((n >> 12) & 63) as usize];
        out[op + 2] = B64[((n >> 6) & 63) as usize];
        out[op + 3] = b'=';
        op += 4;
    }
    op
}

// ---- HTTP date, computed by hand from the unix clock --------------------

const DAYS: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
const MONTHS: [&[u8; 3]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

fn http_date_unix(secs: u64, out: &mut [u8; 29]) {
    let days = (secs / 86_400) as i64;
    let time = (secs % 86_400) as i64;
    let wd = ((days + 4) % 7) as usize; // 1970-01-01 was a Thursday; 0 = Sunday
    // civil date from day count (Hinnant's algorithm), all integer math
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    let mut o = Out::new(&mut out[..]);
    o.push(DAYS[wd]);
    o.push(b", ");
    push2(&mut o, d as u64);
    o.push(b" ");
    o.push(MONTHS[(m - 1) as usize]);
    o.push(b" ");
    push4(&mut o, y as u64);
    o.push(b" ");
    push2(&mut o, (time / 3600) as u64);
    o.push(b":");
    push2(&mut o, ((time / 60) % 60) as u64);
    o.push(b":");
    push2(&mut o, (time % 60) as u64);
    o.push(b" GMT");
}

fn date_now(out: &mut [u8; 29]) {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => http_date_unix(d.as_secs(), out),
        Err(_) => {
            let mut o = Out::new(&mut out[..]);
            o.push(b"Thu, 01 Jan 1970 00:00:00 GMT");
        }
    }
}

// ---- request parsing: everything borrows from the read buffer ----------

#[derive(Clone, Copy)]
struct Header<'a> {
    name: &'a str,
    value: &'a str,
}

struct Request<'a> {
    method: &'a str,
    target: &'a str,
    version: u8, // 0 or 1
    headers: [Option<Header<'a>>; HDR_MAX],
    header_count: usize,
    body: &'a [u8],
}

enum Parse<'a> {
    NeedMore,
    Ready(Request<'a>),
    Fail(u16),
}

fn trim(b: &[u8]) -> &[u8] {
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

fn split_request_line(line: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
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

fn parse_head(head: &[u8]) -> Parse<'_> {
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

fn header<'a>(req: &'a Request<'a>, name: &str) -> Option<&'a str> {
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

fn parse_usize(s: &str) -> Option<usize> {
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
fn content_length_of(head: &[u8]) -> Result<usize, u16> {
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
fn path_of(target: &str) -> &str {
    match target.find('?') {
        Some(q) => &target[..q],
        None => target,
    }
}

// true when any Transfer-Encoding header lists the "chunked" token.
// Comma-separated values are honored; matching is case-insensitive.
fn is_chunked(head: &[u8]) -> bool {
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
fn parse_chunk_size(line: &[u8]) -> Result<usize, u16> {
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
fn read_line(
    stream: &mut TcpStream,
    buf: &mut [u8],
    floor: usize,
    pos: &mut usize,
    n: &mut usize,
    line: &mut [u8],
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
        match stream.read(&mut buf[*n..]) {
            Ok(0) => return Err(400),
            Ok(k) => *n += k,
            Err(_) => return Err(400),
        }
    }
}

// Consume exactly one CRLF from the stream. Ok(false) means the bytes
// were present but were not a CRLF.
fn expect_crlf(
    stream: &mut TcpStream,
    buf: &mut [u8],
    floor: usize,
    pos: &mut usize,
    n: &mut usize,
) -> Result<bool, u16> {
    while *n - *pos < 2 {
        if *n == buf.len() {
            buf.copy_within(*pos..*n, floor);
            *n = floor + (*n - *pos);
            *pos = floor;
        }
        match stream.read(&mut buf[*n..]) {
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
fn decode_chunked(
    stream: &mut TcpStream,
    buf: &mut [u8],
    floor: usize,
    pos: &mut usize,
    n: &mut usize,
    out: &mut [u8],
) -> Result<usize, u16> {
    let mut dlen = 0usize;
    let mut line = [0u8; 64];
    loop {
        let llen = read_line(stream, buf, floor, pos, n, &mut line)?;
        let size = parse_chunk_size(&line[..llen])?;
        if size == 0 {
            // final chunk: swallow trailers until the empty line
            loop {
                let tl = read_line(stream, buf, floor, pos, n, &mut line)?;
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
                match stream.read(&mut buf[*n..]) {
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
        if !expect_crlf(stream, buf, floor, pos, n)? {
            return Err(400);
        }
    }
}

fn keep_alive(req: &Request) -> bool {
    let conn = header(req, "connection");
    let wants_close = matches!(conn, Some(v) if v.eq_ignore_ascii_case("close"));
    let wants_keep = matches!(conn, Some(v) if v.eq_ignore_ascii_case("keep-alive"));
    match req.version {
        1 => !wants_close,
        0 => wants_keep,
        _ => false,
    }
}

fn find_double_crlf(hay: &[u8]) -> Option<usize> {
    hay.windows(4).position(|w| w == b"\r\n\r\n")
}

// ---- routing ------------------------------------------------------------

const INDEX: &str = "<!doctype html><html><head><title>zahttp</title><style>body{background:#0d0d0f;color:#c9a0ff;font-family:monospace;max-width:640px;margin:4rem auto;padding:0 1rem}h1{font-size:3rem}a{color:#7df9ff}</style></head><body><h1>zahttp &#x1f921;</h1><p>zero-dependency, zero-allocation HTTP/1.1. every byte on the stack.</p><ul><li><a href=\"/health\">/health</a></li><li><a href=\"/time\">/time</a></li><li><a href=\"/headers\">/headers</a></li><li><a href=\"/metrics\">/metrics</a></li><li><a href=\"/allocs\">/allocs</a></li><li><a href=\"/chunked\">/chunked</a> (chunked stream)</li><li><code>/ws</code> (websocket)</li><li><a href=\"/bytes\">/bytes</a> (ranges, conditionals, gzip)</li><li><code>/upload</code> (multipart/form-data)</li></ul><p>POST a body to <code>/echo</code> and get it back. Chunked request bodies welcome.</p></body></html>";

// ---- OPTIONS (RFC 7231 4.2.7) -------------------------------------------
// The methods each resource actually speaks; OPTIONS reports them in
// Allow. `*` asks about the server as a whole.
fn allow_for(path: &str) -> Option<&'static str> {
    Some(match path {
        "*" => "GET, HEAD, POST, OPTIONS",
        "/" | "/health" => "GET, HEAD, OPTIONS",
        "/bytes" => "GET, HEAD, OPTIONS",
        "/metrics" | "/allocs" | "/time" | "/headers" => "GET, OPTIONS",
        "/echo" | "/upload" => "POST, OPTIONS",
        "/chunked" | "/ws" => "GET, OPTIONS",
        _ => return None,
    })
}

fn send_options(stream: &mut TcpStream, req: &Request, keep_alive: bool) -> bool {
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

fn route(req: &Request, out: &mut Out) -> (u16, &'static str) {
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
        || path == "/upload";
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

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

// Write buffers as one writev(2) instead of one write(2) per buffer -
// same bytes on the wire, fewer syscalls. Loops only on a short write.
fn write_all_vectored(stream: &mut TcpStream, bufs: &mut [IoSlice<'_>]) -> bool {
    let mut bufs = bufs;
    while !bufs.is_empty() {
        match stream.write_vectored(bufs) {
            Ok(0) => return false,
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    true
}

fn send(
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
    let mut bufs = [IoSlice::new(h.as_slice()), IoSlice::new(payload)];
    write_all_vectored(stream, &mut bufs)
}

// Stream a generated body with Transfer-Encoding: chunked. 64 chunks of
// deterministic LCG hex, each framed as <hexlen>\r\n<payload>\r\n,
// terminated by 0\r\n\r\n. No Content-Length, all fixed buffers.
fn send_chunked(stream: &mut TcpStream, keep_alive: bool) -> bool {
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
        let mut bufs = [
            IoSlice::new(c.as_slice()),
            IoSlice::new(payload),
            IoSlice::new(b"\r\n"),
        ];
        if !write_all_vectored(stream, &mut bufs) {
            return false;
        }
        i += 1;
    }
    stream.write_all(b"0\r\n\r\n").is_ok()
}

// ---- Range requests (RFC 7233): GET/HEAD /bytes --------------------------
// Serves a fixed 64 KiB deterministic body with single, open-ended, suffix,
// and multi-range support. Zero heap: the body is computed once into a
// LazyLock (no allocator involved) and every response borrows slices of it.

const RANGE_TOTAL: usize = 65536;

static RANGE_BODY: LazyLock<[u8; RANGE_TOTAL]> = LazyLock::new(|| {
    let mut b = [0u8; RANGE_TOTAL];
    let mut x = 0x243F6A88u32;
    let mut i = 0;
    while i < RANGE_TOTAL {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        b[i] = (x >> 24) as u8;
        i += 1;
    }
    b
});

const RANGE_ETAG: &str = "\"zahttp-bytes-v1\"";
const RANGE_BOUNDARY: &str = "zahttp-range-7f3a9c";
const MAX_RANGES: usize = 8;
// Part-header literals shared between the writer and the length math so the
// two can never drift apart again.
const PART_CT: &str = "Content-Type: application/octet-stream\r\n";
const PART_CR_PRE: &str = "Content-Range: bytes "; // + F + "-" + L + "/" + T + "\r\n"

fn parse_u64b(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

// Parse one byte-range-spec into an inclusive (first, last). False when
// malformed or unsatisfiable against `total`.
fn parse_one_range(spec: &[u8], total: u64, out: &mut (u64, u64)) -> bool {
    let dash = match spec.iter().position(|&c| c == b'-') {
        Some(p) => p,
        None => return false,
    };
    let (first, last) = (trim(&spec[..dash]), trim(&spec[dash + 1..]));
    if first.is_empty() {
        // suffix-byte-range-spec: the last N bytes
        let n = match parse_u64b(last) {
            Some(n) => n,
            None => return false,
        };
        if n == 0 || total == 0 {
            return false;
        }
        let n = n.min(total);
        out.0 = total - n;
        out.1 = total - 1;
        true
    } else {
        let f = match parse_u64b(first) {
            Some(f) => f,
            None => return false,
        };
        if f >= total {
            return false; // unsatisfiable
        }
        if last.is_empty() {
            out.0 = f;
            out.1 = total - 1;
            true
        } else {
            let l = match parse_u64b(last) {
                Some(l) => l,
                None => return false,
            };
            if l < f {
                return false;
            }
            out.0 = f;
            out.1 = l.min(total - 1); // clamp to the representation
            true
        }
    }
}

// Parse "bytes=r1, r2, ...". Ok(n) fills out[..n]; Err on malformed,
// unsatisfiable, or too many ranges.
fn parse_ranges(value: &str, total: u64, out: &mut [(u64, u64); MAX_RANGES]) -> Result<usize, ()> {
    let v = trim(value.as_bytes());
    if v.len() < 6 || !v[..6].eq_ignore_ascii_case(b"bytes=") {
        return Err(());
    }
    let mut rest = trim(&v[6..]);
    if rest.is_empty() {
        return Err(());
    }
    let mut n = 0;
    while !rest.is_empty() {
        if n == MAX_RANGES {
            return Err(());
        }
        let (spec, next) = match rest.iter().position(|&c| c == b',') {
            Some(p) => (trim(&rest[..p]), trim(&rest[p + 1..])),
            None => (trim(rest), &rest[rest.len()..]),
        };
        let mut r = (0u64, 0u64);
        if spec.is_empty() || !parse_one_range(spec, total, &mut r) {
            return Err(());
        }
        out[n] = r;
        n += 1;
        rest = next;
    }
    Ok(n)
}

fn digits_u64(mut v: u64) -> u64 {
    let mut d = 1;
    while v >= 10 {
        v /= 10;
        d += 1;
    }
    d
}

// Byte length of one multipart/byteranges part for (first, last). Every
// literal length comes from the same consts the writer uses.
fn part_len(first: u64, last: u64, total: u64) -> u64 {
    let b = RANGE_BOUNDARY.len() as u64;
    (2 + b + 2) // --boundary\r\n
        + PART_CT.len() as u64
        + (PART_CR_PRE.len() as u64 + digits_u64(first) + 1 + digits_u64(last) + 1 + digits_u64(total) + 2)
        + 2 // blank line
        + (last - first + 1) // body
        + 2 // \r\n
}

fn range_head_start(h: &mut Out, status: u16) {
    let mut date = [0u8; 29];
    date_now(&mut date);
    h.push_str("HTTP/1.1 ");
    h.push_u64(status as u64);
    h.push_str(" ");
    h.push_str(reason(status));
    h.push_str("\r\nDate: ");
    h.push(&date);
    h.push_str("\r\nServer: zahttp/0.1\r\n");
}

fn range_head_end(h: &mut Out, content_length: u64, keep_alive: bool) {
    h.push_str("Content-Length: ");
    h.push_u64(content_length);
    h.push_str("\r\nConnection: ");
    h.push_str(if keep_alive { "keep-alive" } else { "close" });
    h.push_str("\r\n\r\n");
}

fn send_range_full(stream: &mut TcpStream, keep_alive: bool, with_body: bool, etag: &str) -> bool {
    let mut hbuf = [0u8; 384];
    let mut h = Out::new(&mut hbuf);
    range_head_start(&mut h, 200);
    h.push_str("Content-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\nETag: ");
    h.push_str(etag);
    h.push_str("\r\nVary: Accept-Encoding\r\n");
    range_head_end(&mut h, RANGE_TOTAL as u64, keep_alive);
    if h.overflow {
        return false;
    }
    let payload: &[u8] = if with_body { &RANGE_BODY[..] } else { &[] };
    let mut bufs = [IoSlice::new(h.as_slice()), IoSlice::new(payload)];
    write_all_vectored(stream, &mut bufs)
}

fn send_gzip_full(stream: &mut TcpStream, keep_alive: bool, with_body: bool) -> bool {
    let gz = &*GZIP_BYTES;
    let mut hbuf = [0u8; 384];
    let mut h = Out::new(&mut hbuf);
    range_head_start(&mut h, 200);
    h.push_str(
        "Content-Type: application/octet-stream\r\nContent-Encoding: gzip\r\nAccept-Ranges: bytes\r\nETag: ",
    );
    h.push_str(RANGE_ETAG_GZIP);
    h.push_str("\r\nVary: Accept-Encoding\r\n");
    range_head_end(&mut h, gz.0 as u64, keep_alive);
    if h.overflow {
        return false;
    }
    let payload: &[u8] = if with_body { &gz.1[..gz.0] } else { &[] };
    let mut bufs = [IoSlice::new(h.as_slice()), IoSlice::new(payload)];
    write_all_vectored(stream, &mut bufs)
}

fn send_range_single(
    stream: &mut TcpStream,
    keep_alive: bool,
    with_body: bool,
    first: u64,
    last: u64,
    etag: &str,
) -> bool {
    let total = RANGE_TOTAL as u64;
    let mut hbuf = [0u8; 384];
    let mut h = Out::new(&mut hbuf);
    range_head_start(&mut h, 206);
    h.push_str("Content-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\nETag: ");
    h.push_str(etag);
    h.push_str("\r\nContent-Range: bytes ");
    h.push_u64(first);
    h.push_str("-");
    h.push_u64(last);
    h.push_str("/");
    h.push_u64(total);
    h.push_str("\r\n");
    range_head_end(&mut h, last - first + 1, keep_alive);
    if h.overflow {
        return false;
    }
    let payload: &[u8] = if with_body {
        &RANGE_BODY[first as usize..=last as usize]
    } else {
        &[]
    };
    let mut bufs = [IoSlice::new(h.as_slice()), IoSlice::new(payload)];
    write_all_vectored(stream, &mut bufs)
}

fn send_range_multi(
    stream: &mut TcpStream,
    keep_alive: bool,
    with_body: bool,
    rs: &[(u64, u64)],
    etag: &str,
) -> bool {
    let total = RANGE_TOTAL as u64;
    let b = RANGE_BOUNDARY;
    let mut content_length: u64 = 0;
    for &(first, last) in rs {
        content_length += part_len(first, last, total);
    }
    content_length += 2 + b.len() as u64 + 4; // --boundary--\r\n
    let mut hbuf = [0u8; 384];
    let mut h = Out::new(&mut hbuf);
    range_head_start(&mut h, 206);
    h.push_str("Content-Type: multipart/byteranges; boundary=");
    h.push_str(b);
    h.push_str("\r\nAccept-Ranges: bytes\r\nETag: ");
    h.push_str(etag);
    h.push_str("\r\n");
    range_head_end(&mut h, content_length, keep_alive);
    // multipart/byteranges never combines with gzip (Range wins), so no Vary.
    if h.overflow {
        return false;
    }
    if !stream.write_all(h.as_slice()).is_ok() {
        return false;
    }
    if with_body {
        let mut pbuf = [0u8; 160];
        for &(first, last) in rs {
            let mut p = Out::new(&mut pbuf);
            p.push_str("--");
            p.push_str(b);
            p.push_str("\r\n");
            p.push_str(PART_CT);
            p.push_str(PART_CR_PRE);
            p.push_u64(first);
            p.push_str("-");
            p.push_u64(last);
            p.push_str("/");
            p.push_u64(total);
            p.push_str("\r\n\r\n");
            if p.overflow {
                return false;
            }
            if !stream.write_all(p.as_slice()).is_ok() {
                return false;
            }
            if !stream
                .write_all(&RANGE_BODY[first as usize..=last as usize])
                .is_ok()
            {
                return false;
            }
            if !stream.write_all(b"\r\n").is_ok() {
                return false;
            }
        }
        let mut p = Out::new(&mut pbuf);
        p.push_str("--");
        p.push_str(b);
        p.push_str("--\r\n");
        if p.overflow {
            return false;
        }
        if !stream.write_all(p.as_slice()).is_ok() {
            return false;
        }
    }
    true
}

fn send_not_modified(stream: &mut TcpStream, keep_alive: bool, etag: &str) -> bool {
    let mut hbuf = [0u8; 384];
    let mut h = Out::new(&mut hbuf);
    range_head_start(&mut h, 304);
    h.push_str("ETag: ");
    h.push_str(etag);
    h.push_str("\r\nVary: Accept-Encoding\r\nConnection: ");
    h.push_str(if keep_alive { "keep-alive" } else { "close" });
    h.push_str("\r\n\r\n");
    // 304 never carries a body, so no Content-Length is needed for framing.
    !h.overflow && stream.write_all(h.as_slice()).is_ok()
}

// RFC 7232 2.3.2 + 3.3: If-None-Match uses the weak comparison function.
// `*` matches any current representation; W/"..." matches weakly.
fn etag_list_matches(value: &str, etag: &str) -> bool {
    let bytes = value.as_bytes();
    let mut start = 0usize;
    loop {
        let mut end = start;
        while end < bytes.len() && bytes[end] != b',' {
            end += 1;
        }
        let seg = trim(&bytes[start..end]);
        if seg == b"*" {
            return true;
        }
        let tag = match seg.strip_prefix(b"W/") {
            Some(t) => t,
            None => seg,
        };
        if tag == etag.as_bytes() {
            return true;
        }
        if end == bytes.len() {
            return false;
        }
        start = end + 1;
    }
}

fn send_ranges(stream: &mut TcpStream, req: &Request, keep_alive: bool) -> bool {
    let total = RANGE_TOTAL as u64;
    let with_body = req.method == "GET"; // HEAD: headers only
    // Content negotiation (RFC 7231 3.1.2.2): gzip only for full-body 200s,
    // never combined with Range — like nginx, Range wins over encoding.
    let gzip = header(req, "range").is_none() && accepts_gzip(req) && GZIP_BYTES.0 > 0;
    let etag = if gzip { RANGE_ETAG_GZIP } else { RANGE_ETAG };
    // RFC 7232 3.3: a matching If-None-Match short-circuits everything,
    // including Range, with 304 Not Modified.
    if let Some(v) = header(req, "if-none-match") {
        if etag_list_matches(v, etag) {
            return send_not_modified(stream, keep_alive, etag);
        }
    }
    let mut rs = [(0u64, 0u64); MAX_RANGES];
    let nranges = match header(req, "range") {
        Some(v) => {
            let if_range_ok = match header(req, "if-range") {
                Some(iv) => iv.trim() == RANGE_ETAG,
                None => true,
            };
            if !if_range_ok {
                0 // stale validator: ignore Range, serve 200
            } else {
                match parse_ranges(v, total, &mut rs) {
                    Ok(n) => n,
                    Err(()) => {
                        let mut hbuf = [0u8; 384];
                        let mut h = Out::new(&mut hbuf);
                        range_head_start(&mut h, 416);
                        h.push_str("Content-Range: bytes */");
                        h.push_u64(total);
                        h.push_str("\r\n");
                        range_head_end(&mut h, 0, keep_alive);
                        return !h.overflow && stream.write_all(h.as_slice()).is_ok();
                    }
                }
            }
        }
        None => 0,
    };
    if nranges == 0 {
        if gzip {
            send_gzip_full(stream, keep_alive, with_body)
        } else {
            send_range_full(stream, keep_alive, with_body, etag)
        }
    } else if nranges == 1 {
        send_range_single(stream, keep_alive, with_body, rs[0].0, rs[0].1, etag)
    } else {
        send_range_multi(stream, keep_alive, with_body, &rs[..nranges], etag)
    }
}

// What the client expects before sending the body (RFC 7231 5.1.1).
#[derive(PartialEq, Eq)]
enum Expect {
    None,
    Continue,
    Other,
}

// Scan the raw head for an Expect header. HTTP/1.0 clients never get a
// 100-continue: the interim status would confuse their framing.
fn expect_of(head: &[u8]) -> Expect {
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

fn send_100(stream: &mut TcpStream) -> bool {
    stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_ok()
}

// ---- gzip content-coding (RFC 1952): the unit ---------------------------
// Hand-rolled DEFLATE with fixed Huffman codes (RFC 1951 3.2.6): LZ77 with
// 3-byte hash chains over a 32 KiB window, greedy matches, an LSB-first
// bit writer, and a table-free CRC32. Everything over fixed stack buffers;
// the 64 KiB body is compressed once into a LazyLock and every response
// borrows slices of it.

// worst case: 64 KiB of literals at 9 bits each + framing
const GZIP_CAP: usize = 74752;
const GZIP_MAX_IN: usize = 65536;

struct BitW<'a> {
    out: &'a mut [u8],
    pos: usize,
    acc: u32,   // pending bits, LSB-first
    nbits: u32, // how many are pending
    ok: bool,
}

impl<'a> BitW<'a> {
    fn bits(&mut self, value: u32, count: u32) {
        // every call site uses count <= 13, so acc never overflows u32
        self.acc |= (value & ((1u32 << count) - 1)) << self.nbits;
        self.nbits += count;
        while self.nbits >= 8 {
            if self.pos >= self.out.len() {
                self.ok = false;
                return;
            }
            self.out[self.pos] = self.acc as u8;
            self.pos += 1;
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }
    fn flush(&mut self) {
        if self.nbits > 0 {
            if self.pos >= self.out.len() {
                self.ok = false;
                return;
            }
            self.out[self.pos] = self.acc as u8;
            self.pos += 1;
            self.acc = 0;
            self.nbits = 0;
        }
    }
}

fn rev_bits(mut v: u32, mut n: u32) -> u32 {
    let mut r = 0u32;
    while n > 0 {
        r = (r << 1) | (v & 1);
        v >>= 1;
        n -= 1;
    }
    r
}

// fixed Huffman literal/length code -> (code, bit length); codes are
// MSB-first here and get bit-reversed at the call site for the wire
fn lit_code(sym: u32) -> (u32, u32) {
    if sym <= 143 {
        (0x30 + sym, 8)
    } else if sym <= 255 {
        (0x190 + (sym - 144), 9)
    } else if sym <= 279 {
        (sym - 256, 7)
    } else {
        (0xC0 + (sym - 280), 8)
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83,
    99, 115, 131, 163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5,
    5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769,
    1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11,
    11, 12, 12, 13, 13,
];

// -> (symbol, extra bits, extra value)
fn len_code(len: u32) -> (u32, u32, u32) {
    let mut i = 0usize;
    while i + 1 < LEN_BASE.len() && (LEN_BASE[i + 1] as u32) <= len {
        i += 1;
    }
    (257 + i as u32, LEN_EXTRA[i] as u32, len - LEN_BASE[i] as u32)
}

fn dist_code(dist: u32) -> (u32, u32, u32) {
    let mut i = 0usize;
    while i + 1 < DIST_BASE.len() && (DIST_BASE[i + 1] as u32) <= dist {
        i += 1;
    }
    (i as u32, DIST_EXTRA[i] as u32, dist - DIST_BASE[i] as u32)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        let mut k = 0;
        while k < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            k += 1;
        }
    }
    !crc
}

fn emit_lit(w: &mut BitW, sym: u32) {
    let (c, n) = lit_code(sym);
    w.bits(rev_bits(c, n), n);
}

fn hash3(b0: u8, b1: u8, b2: u8) -> usize {
    (((b0 as u32) << 10) ^ ((b1 as u32) << 5) ^ (b2 as u32)) as usize & 8191
}

fn gzip_encode(input: &[u8], out: &mut [u8]) -> Option<usize> {
    if input.len() > GZIP_MAX_IN || out.len() < 18 {
        return None;
    }
    // gzip header: magic, deflate method, no flags, mtime 0, xfl 0, OS = unix
    const HDR: [u8; 10] = [0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03];
    out[..10].copy_from_slice(&HDR);
    let mut w = BitW {
        out: &mut out[10..],
        pos: 0,
        acc: 0,
        nbits: 0,
        ok: true,
    };
    w.bits(1, 1); // BFINAL
    w.bits(0b01, 2); // BTYPE = fixed Huffman
    const HN: usize = 8192;
    let mut head = [0u32; HN]; // hash -> newest position + 1 (0 = none)
    let mut prev = [0u32; GZIP_MAX_IN]; // position -> older position + 1
    let n = input.len();
    let mut i = 0usize;
    while i < n {
        let mut best_len = 0u32;
        let mut best_dist = 0u32;
        if i + 3 <= n {
            let h = hash3(input[i], input[i + 1], input[i + 2]);
            let mut cand = head[h];
            let mut probes = 0u32;
            let max_dist = i.min(32768) as u32;
            while cand != 0 && probes < 128 {
                probes += 1;
                let p = (cand - 1) as usize;
                let dist = (i - p) as u32;
                if dist > max_dist {
                    break; // chains run newest-first; all later are farther
                }
                let mut len = 0u32;
                while len < 258 && i + (len as usize) < n && input[p + (len as usize)] == input[i + (len as usize)] {
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_dist = dist;
                    if len == 258 {
                        break;
                    }
                }
                cand = prev[p];
            }
            prev[i] = head[h];
            head[h] = (i + 1) as u32;
        }
        if best_len >= 3 {
            let (sym, eb, ev) = len_code(best_len);
            emit_lit(&mut w, sym);
            if eb > 0 {
                w.bits(ev, eb);
            }
            let (dsym, deb, dev) = dist_code(best_dist);
            w.bits(rev_bits(dsym, 5), 5);
            if deb > 0 {
                w.bits(dev, deb);
            }
            // index the skipped positions so later matches can overlap
            let mut k = 1usize;
            while k < best_len as usize {
                let j = i + k;
                if j + 3 <= n {
                    let h = hash3(input[j], input[j + 1], input[j + 2]);
                    prev[j] = head[h];
                    head[h] = (j + 1) as u32;
                }
                k += 1;
            }
            i += best_len as usize;
        } else {
            emit_lit(&mut w, input[i] as u32);
            i += 1;
        }
        if !w.ok {
            return None;
        }
    }
    emit_lit(&mut w, 256); // end of block
    w.flush();
    if !w.ok {
        return None;
    }
    let mut pos = 10 + w.pos;
    if pos + 8 > out.len() {
        return None;
    }
    out[pos..pos + 4].copy_from_slice(&crc32(input).to_le_bytes());
    pos += 4;
    out[pos..pos + 4].copy_from_slice(&(n as u32).to_le_bytes());
    pos += 4;
    Some(pos)
}

// Compressed once on first use; every gzip response borrows slices of it.
static GZIP_BYTES: LazyLock<(usize, [u8; GZIP_CAP])> = LazyLock::new(|| {
    let mut buf = [0u8; GZIP_CAP];
    match gzip_encode(&RANGE_BODY[..], &mut buf) {
        Some(n) => (n, buf),
        None => (0, buf),
    }
});

const RANGE_ETAG_GZIP: &str = "\"zahttp-bytes-v1+gzip\"";

// true when the client will take a gzip content-coding (RFC 7231 5.3.4).
// Comma-separated tokens; `gzip;q=0` opts out, `*` (with q>0) counts.
fn q_is_zero(s: &[u8]) -> bool {
    let mut it = s.iter();
    match it.next() {
        Some(b'0') => {}
        _ => return false,
    }
    match it.next() {
        None => return true,
        Some(b'.') => {}
        _ => return false,
    }
    it.all(|&c| c == b'0')
}

fn qvalue_zero(params: &[u8]) -> bool {
    // params starts at the first ';' of the token (or is empty)
    let mut i = 0usize;
    while i < params.len() {
        if params[i] == b';' {
            let mut j = i + 1;
            while j < params.len() && params[j] == b' ' {
                j += 1;
            }
            if j + 1 < params.len() && (params[j] | 0x20) == b'q' && params[j + 1] == b'=' {
                let mut k = j + 2;
                while k < params.len() && params[k] == b' ' {
                    k += 1;
                }
                let mut e = k;
                while e < params.len() && params[e] != b';' && params[e] != b' ' {
                    e += 1;
                }
                return q_is_zero(&params[k..e]);
            }
        }
        i += 1;
    }
    false
}

fn accepts_gzip(req: &Request) -> bool {
    let v = match header(req, "accept-encoding") {
        Some(v) => v,
        None => return false,
    };
    let bytes = v.as_bytes();
    let mut start = 0usize;
    loop {
        let mut end = start;
        while end < bytes.len() && bytes[end] != b',' {
            end += 1;
        }
        let seg = trim(&bytes[start..end]);
        let te = seg.iter().position(|&c| c == b';').unwrap_or(seg.len());
        let tok = trim(&seg[..te]);
        if tok.eq_ignore_ascii_case(b"gzip") || tok == b"*" {
            if !qvalue_zero(&seg[te..]) {
                return true;
            }
        }
        if end == bytes.len() {
            return false;
        }
        start = end + 1;
    }
}

// ---- multipart/form-data parsing (RFC 7578): POST /upload --------------
// Parses the request body in place. Zero heap: part descriptors live in a
// fixed array and every string borrows from the body buffer. Documented
// simplifications: no backslash-escape processing inside quoted values,
// naive boundary search (a boundary chosen to collide with content will
// confuse any simple parser).

const MAX_PARTS: usize = 16;
const MAX_BOUNDARY: usize = 128;

#[derive(Clone, Copy)]
struct FormPart<'b> {
    name: &'b [u8],
    filename: Option<&'b [u8]>,
    content_type: &'b [u8],
    body: &'b [u8],
}

impl<'b> FormPart<'b> {
    const EMPTY: FormPart<'b> = FormPart {
        name: b"",
        filename: None,
        content_type: b"text/plain",
        body: b"",
    };
}

// Byte-level memmem.
fn memfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// Does `needle` occur at `body[pos..]`? Bounds-safe.
fn at(body: &[u8], pos: usize, needle: &[u8]) -> bool {
    body.len() >= pos + needle.len() && &body[pos..pos + needle.len()] == needle
}

// Find a `key=value` (or `key="quoted value"`) parameter in a `;`-separated
// header value. The key must start at a parameter boundary so `name` never
// matches inside `filename`.
fn param<'b>(hv: &'b [u8], key: &[u8]) -> Option<&'b [u8]> {
    let mut i = 0;
    while i + key.len() <= hv.len() {
        if hv[i..i + key.len()].eq_ignore_ascii_case(key)
            && (i == 0 || hv[i - 1] == b';' || hv[i - 1] == b' ' || hv[i - 1] == b'\t')
        {
            let mut j = i + key.len();
            while j < hv.len() && (hv[j] == b' ' || hv[j] == b'\t') {
                j += 1;
            }
            if j < hv.len() && hv[j] == b'=' {
                j += 1;
                while j < hv.len() && (hv[j] == b' ' || hv[j] == b'\t') {
                    j += 1;
                }
                if j < hv.len() && hv[j] == b'"' {
                    let k = memfind(&hv[j + 1..], b"\"")?;
                    return Some(&hv[j + 1..j + 1 + k]);
                }
                let mut k = j;
                while k < hv.len()
                    && hv[k] != b';'
                    && hv[k] != b' '
                    && hv[k] != b'\t'
                    && hv[k] != b'\r'
                    && hv[k] != b'\n'
                {
                    k += 1;
                }
                return Some(&hv[j..k]);
            }
        }
        i += 1;
    }
    None
}

fn parse_part_headers<'b>(head: &'b [u8]) -> (&'b [u8], Option<&'b [u8]>, &'b [u8]) {
    let mut name: &[u8] = b"";
    let mut filename: Option<&[u8]> = None;
    let mut ctype: &[u8] = b"text/plain";
    let mut rest = head;
    while !rest.is_empty() {
        let (line, next) = match memfind(rest, b"\r\n") {
            Some(p) => (&rest[..p], &rest[p + 2..]),
            None => (rest, &rest[rest.len()..]),
        };
        rest = next;
        let colon = match line.iter().position(|&c| c == b':') {
            Some(p) => p,
            None => continue,
        };
        let (hn, hv) = (trim(&line[..colon]), trim(&line[colon + 1..]));
        if hn.eq_ignore_ascii_case(b"content-disposition") {
            name = param(hv, b"name").unwrap_or(b"");
            filename = param(hv, b"filename");
        } else if hn.eq_ignore_ascii_case(b"content-type") {
            ctype = match memfind(hv, b";") {
                Some(p) => trim(&hv[..p]),
                None => hv,
            };
        }
    }
    (name, filename, ctype)
}

fn multipart_boundary<'a>(req: &'a Request<'a>) -> Option<&'a [u8]> {
    let ct = header(req, "content-type")?;
    let cb = ct.as_bytes();
    if cb.len() < 19 || !cb[..19].eq_ignore_ascii_case(b"multipart/form-data") {
        return None;
    }
    let b = param(cb, b"boundary")?;
    if b.is_empty() || b.len() > MAX_BOUNDARY {
        return None;
    }
    Some(b)
}

// Split the body into parts. Ok(n) fills parts[..n]; Err(status) on
// malformed framing (400) or too many parts (413).
fn parse_multipart<'b>(
    body: &'b [u8],
    boundary: &[u8],
    parts: &mut [FormPart<'b>],
) -> Result<usize, u16> {
    let mut dbuf = [0u8; MAX_BOUNDARY + 2];
    dbuf[0] = b'-';
    dbuf[1] = b'-';
    dbuf[2..2 + boundary.len()].copy_from_slice(boundary);
    let delim = &dbuf[..2 + boundary.len()];
    if !at(body, 0, delim) {
        return Err(400);
    }
    let mut pos = delim.len();
    if at(body, pos, b"--") {
        return Ok(0); // immediate close: zero parts
    }
    if !at(body, pos, b"\r\n") {
        return Err(400);
    }
    pos += 2;
    let mut nbuf = [0u8; MAX_BOUNDARY + 4];
    nbuf[0] = b'\r';
    nbuf[1] = b'\n';
    nbuf[2..2 + delim.len()].copy_from_slice(delim);
    let needle = &nbuf[..2 + delim.len()];
    let mut n = 0;
    loop {
        if pos > body.len() {
            return Err(400);
        }
        if n == parts.len() {
            return Err(413);
        }
        let hend = match memfind(&body[pos..], b"\r\n\r\n") {
            Some(p) => pos + p,
            None => return Err(400),
        };
        let (name, filename, ctype) = parse_part_headers(&body[pos..hend]);
        let bstart = hend + 4;
        let rel = match memfind(&body[bstart..], needle) {
            Some(p) => p,
            None => return Err(400),
        };
        let bend = bstart + rel;
        parts[n] = FormPart {
            name,
            filename,
            content_type: ctype,
            body: &body[bstart..bend],
        };
        n += 1;
        pos = bend + needle.len();
        if pos > body.len() {
            return Err(400);
        }
        if at(body, pos, b"--") {
            return Ok(n); // closing delimiter; epilogue ignored
        }
        if !at(body, pos, b"\r\n") {
            return Err(400);
        }
        pos += 2;
    }
}

fn serve_upload(req: &Request, out: &mut Out) -> Result<(), u16> {
    let boundary = match multipart_boundary(req) {
        Some(b) => b,
        None => {
            out.push_str("bad multipart boundary\n");
            return Err(400);
        }
    };
    let mut parts: [FormPart; MAX_PARTS] = [FormPart::EMPTY; MAX_PARTS];
    let n = match parse_multipart(req.body, boundary, &mut parts) {
        Ok(n) => n,
        Err(s) => {
            out.push_str(if s == 413 {
                "too many parts\n"
            } else {
                "malformed multipart body\n"
            });
            return Err(s);
        }
    };
    out.push_str("parts: ");
    out.push_u64(n as u64);
    out.push_str("\n");
    let mut i = 0;
    while i < n {
        let p = &parts[i];
        out.push_str("part ");
        out.push_u64(i as u64);
        out.push_str(": name=\"");
        out.push(p.name);
        out.push_str("\" filename=\"");
        match p.filename {
            Some(f) => out.push(f),
            None => out.push_str("-"),
        }
        out.push_str("\" type=\"");
        out.push(p.content_type);
        out.push_str("\" size=");
        out.push_u64(p.body.len() as u64);
        out.push_str("\n");
        i += 1;
    }
    Ok(())
}

// ---- WebSocket (RFC 6455): handshake + frame codec, zero heap -----------

const WS_MSG_CAP: usize = 4096;
const WS_GUID: &[u8; 36] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// True when any header named `name` carries `token` in a comma-separated
// list. Case-insensitive, per the HTTP token rules.
fn header_has_token(req: &Request, name: &str, token: &[u8]) -> bool {
    let mut i = 0;
    while i < req.header_count {
        if let Some(hdr) = req.headers[i] {
            if hdr.name.eq_ignore_ascii_case(name) {
                let mut tok = trim(hdr.value.as_bytes());
                while !tok.is_empty() {
                    let (t, next) = match tok.iter().position(|&c| c == b',') {
                        Some(p) => (&tok[..p], &tok[p + 1..]),
                        None => (tok, &tok[tok.len()..]),
                    };
                    if trim(t).eq_ignore_ascii_case(token) {
                        return true;
                    }
                    tok = next;
                }
            }
        }
        i += 1;
    }
    false
}

fn ws_reject(stream: &mut TcpStream, status: u16, reason: &str, version_header: bool) {
    let mut hbuf = [0u8; 256];
    let mut h = Out::new(&mut hbuf);
    h.push_str("HTTP/1.1 ");
    h.push_u64(status as u64);
    h.push_str(" ");
    h.push_str(reason);
    h.push_str("\r\nContent-Type: text/plain\r\nContent-Length: 0\r\n");
    if version_header {
        h.push_str("Sec-WebSocket-Version: 13\r\n");
    }
    h.push_str("Connection: close\r\n\r\n");
    if !h.overflow {
        let _ = stream.write_all(h.as_slice());
    }
}

// Read exactly buf.len() bytes; false on EOF or error.
fn read_full(stream: &mut TcpStream, buf: &mut [u8]) -> bool {
    let mut off = 0;
    while off < buf.len() {
        match stream.read(&mut buf[off..]) {
            Ok(0) => return false,
            Ok(k) => off += k,
            Err(_) => return false,
        }
    }
    true
}

// Discard up to `len` payload bytes so our close frame isn't RST-raced by
// unread receive data. Gives up on absurd lengths; caller closes regardless.
fn ws_drain(stream: &mut TcpStream, len: u64) -> bool {
    if len > 65536 {
        return false;
    }
    let mut chunk = [0u8; 1024];
    let mut left = len;
    while left > 0 {
        let n = (left as usize).min(1024);
        if !read_full(stream, &mut chunk[..n]) {
            return false;
        }
        left -= n as u64;
    }
    true
}

// Send one server-to-client frame (never masked, FIN always set).
fn ws_send(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> bool {
    let mut hbuf = [0u8; 10];
    hbuf[0] = 0x80 | opcode;
    let hlen = if payload.len() < 126 {
        hbuf[1] = payload.len() as u8;
        2
    } else {
        hbuf[1] = 126;
        hbuf[2] = (payload.len() >> 8) as u8;
        hbuf[3] = payload.len() as u8;
        4
    };
    stream.write_all(&hbuf[..hlen]).is_ok() && stream.write_all(payload).is_ok()
}

fn ws_close(stream: &mut TcpStream, code: u16, reason: &[u8]) {
    let mut pbuf = [0u8; 125];
    pbuf[0] = (code >> 8) as u8;
    pbuf[1] = code as u8;
    let rlen = reason.len().min(123);
    pbuf[2..2 + rlen].copy_from_slice(&reason[..rlen]);
    let _ = ws_send(stream, 0x8, &pbuf[..2 + rlen]);
}

fn ws_serve(stream: &mut TcpStream, req: &Request) {
    match header(req, "sec-websocket-version") {
        Some("13") => {}
        Some(_) => {
            ws_reject(stream, 426, "Upgrade Required", true);
            return;
        }
        None => {
            ws_reject(stream, 400, "Bad Request", false);
            return;
        }
    }
    if !header_has_token(req, "upgrade", b"websocket")
        || !header_has_token(req, "connection", b"upgrade")
    {
        ws_reject(stream, 400, "Bad Request", false);
        return;
    }
    let key = match header(req, "sec-websocket-key") {
        Some(k) if !k.is_empty() && k.len() <= 64 => k,
        _ => {
            ws_reject(stream, 400, "Bad Request", false);
            return;
        }
    };
    let mut input = [0u8; 100];
    input[..key.len()].copy_from_slice(key.as_bytes());
    input[key.len()..key.len() + 36].copy_from_slice(WS_GUID);
    let mut digest = [0u8; 20];
    sha1(&input[..key.len() + 36], &mut digest);
    let mut accept = [0u8; 28];
    let alen = base64_encode(&digest, &mut accept);
    let mut hbuf = [0u8; 384];
    let mut h = Out::new(&mut hbuf);
    let mut date = [0u8; 29];
    date_now(&mut date);
    h.push_str("HTTP/1.1 101 Switching Protocols\r\nDate: ");
    h.push(&date);
    h.push_str("\r\nServer: zahttp/0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ");
    h.push(&accept[..alen]);
    h.push_str("\r\n\r\n");
    if h.overflow || !stream.write_all(h.as_slice()).is_ok() {
        return;
    }
    ws_loop(stream);
}

fn ws_loop(stream: &mut TcpStream) {
    let mut msg = [0u8; WS_MSG_CAP];
    let mut mlen = 0usize;
    let mut frag_op = 0u8; // 0 = no fragmented message in flight
    let mut hdr = [0u8; 14];
    loop {
        if !read_full(stream, &mut hdr[..2]) {
            return;
        }
        let fin = hdr[0] & 0x80 != 0;
        let rsv = hdr[0] & 0x70 != 0;
        let opcode = hdr[0] & 0x0F;
        let masked = hdr[1] & 0x80 != 0;
        let len_field = hdr[1] & 0x7F;
        let mut mkey_at = 2usize;
        let mut len = len_field as u64;
        if len_field == 126 {
            if !read_full(stream, &mut hdr[2..4]) {
                return;
            }
            len = ((hdr[2] as u64) << 8) | (hdr[3] as u64);
            mkey_at = 4;
        } else if len_field == 127 {
            if !read_full(stream, &mut hdr[2..10]) {
                return;
            }
            let mut v = 0u64;
            let mut i = 2;
            while i < 10 {
                v = (v << 8) | (hdr[i] as u64);
                i += 1;
            }
            if v >> 63 != 0 {
                ws_close(stream, 1009, b"too big");
                return;
            }
            len = v;
            mkey_at = 10;
        }
        if !masked {
            // No mask key on the wire; drain the payload, then fail the frame.
            ws_drain(stream, len);
            ws_close(stream, 1002, b"protocol");
            return;
        }
        if !read_full(stream, &mut hdr[mkey_at..mkey_at + 4]) {
            return;
        }
        let mask = [hdr[mkey_at], hdr[mkey_at + 1], hdr[mkey_at + 2], hdr[mkey_at + 3]];
        if rsv {
            ws_drain(stream, len);
            ws_close(stream, 1002, b"protocol");
            return;
        }

        if opcode >= 0x8 {
            // control frame: never fragmented, payload <= 125
            if !fin || len > 125 {
                ws_drain(stream, len);
                ws_close(stream, 1002, b"protocol");
                return;
            }
            let len = len as usize;
            let mut cbuf = [0u8; 125];
            if !read_full(stream, &mut cbuf[..len]) {
                return;
            }
            let mut i = 0;
            while i < len {
                cbuf[i] ^= mask[i % 4];
                i += 1;
            }
            match opcode {
                0x8 => {
                    let _ = ws_send(stream, 0x8, &cbuf[..len]);
                    return;
                }
                0x9 => {
                    if !ws_send(stream, 0xA, &cbuf[..len]) {
                        return;
                    }
                }
                0xA => {} // pong: noted, ignored
                _ => {
                    ws_close(stream, 1002, b"protocol");
                    return;
                }
            }
            continue;
        }

        // data frame
        if opcode != 0x0 && opcode != 0x1 && opcode != 0x2 {
            ws_drain(stream, len);
            ws_close(stream, 1002, b"protocol");
            return;
        }
        if opcode == 0x0 {
            if frag_op == 0 {
                ws_drain(stream, len);
                ws_close(stream, 1002, b"protocol");
                return;
            }
        } else {
            if frag_op != 0 {
                ws_drain(stream, len);
                ws_close(stream, 1002, b"protocol");
                return;
            }
            frag_op = opcode;
            mlen = 0;
        }
        if len > (WS_MSG_CAP - mlen) as u64 {
            ws_drain(stream, len);
            ws_close(stream, 1009, b"too big");
            return;
        }
        let len = len as usize;
        if !read_full(stream, &mut msg[mlen..mlen + len]) {
            return;
        }
        let mut i = 0;
        while i < len {
            msg[mlen + i] ^= mask[i % 4];
            i += 1;
        }
        mlen += len;
        if fin {
            if !ws_send(stream, frag_op, &msg[..mlen]) {
                return;
            }
            frag_op = 0;
            mlen = 0;
        }
    }
}

// ---- connection driver --------------------------------------------------

fn serve(mut stream: TcpStream) {
    let mut buf = [0u8; READ_CAP];
    let mut n = 0usize;
    loop {
        // 1. read until the head is complete
        let head_end: usize = loop {
            if let Some(p) = find_double_crlf(&buf[..n]) {
                break p;
            }
            if n == buf.len() {
                send(&mut stream, 431, "text/plain", b"head too large\n", false, true);
                return;
            }
            match stream.read(&mut buf[n..]) {
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
        let mut decoded = [0u8; BODY_CAP];
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
        if chunked {
            if expect == Expect::Continue && !send_100(&mut stream) {
                return;
            }
            let mut pos = body_start;
            let dlen = match decode_chunked(&mut stream, &mut buf, body_start, &mut pos, &mut n, &mut decoded) {
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
                match stream.read(&mut buf[n..]) {
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
        let ka = keep_alive(&req);
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
                thread::spawn(move || serve(s));
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
