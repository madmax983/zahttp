// zahttp module: ranges (RFC 7233 /bytes) — zero deps, zero heap. See main.rs for the rules.

use std::io::{IoSlice, Write};
use std::net::TcpStream;
use std::sync::LazyLock;

use crate::buf::{date_now, parse_u64b, Out};
use crate::routes::reason;
use crate::gzip::{gzip_encode, GZIP_CAP};
use crate::http::{header, trim, write_all_vectored, Request};

// ---- Range requests (RFC 7233): GET/HEAD /bytes --------------------------
// Serves a fixed 64 KiB deterministic body with single, open-ended, suffix,
// and multi-range support. Zero heap: the body is computed once into a
// LazyLock (no allocator involved) and every response borrows slices of it.

pub(crate) const RANGE_TOTAL: usize = 65536;

pub(crate) static RANGE_BODY: LazyLock<[u8; RANGE_TOTAL]> = LazyLock::new(|| {
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
// Compressed once on first use; every gzip response borrows slices of it.
pub(crate) static GZIP_BYTES: LazyLock<(usize, [u8; GZIP_CAP])> = LazyLock::new(|| {
    let mut buf = [0u8; GZIP_CAP];
    match gzip_encode(&RANGE_BODY[..], &mut buf) {
        Some(n) => (n, buf),
        None => (0, buf),
    }
});


pub(crate) const RANGE_ETAG: &str = "\"zahttp-bytes-v1\"";
pub(crate) const RANGE_BOUNDARY: &str = "zahttp-range-7f3a9c";
pub(crate) const MAX_RANGES: usize = 8;
// Part-header literals shared between the writer and the length math so the
// two can never drift apart again.
pub(crate) const PART_CT: &str = "Content-Type: application/octet-stream\r\n";
pub(crate) const PART_CR_PRE: &str = "Content-Range: bytes "; // + F + "-" + L + "/" + T + "\r\n"


// Parse one byte-range-spec into an inclusive (first, last). False when
// malformed or unsatisfiable against `total`.
pub(crate) fn parse_one_range(spec: &[u8], total: u64, out: &mut (u64, u64)) -> bool {
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
pub(crate) fn parse_ranges(value: &str, total: u64, out: &mut [(u64, u64); MAX_RANGES]) -> Result<usize, ()> {
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

pub(crate) fn digits_u64(mut v: u64) -> u64 {
    let mut d = 1;
    while v >= 10 {
        v /= 10;
        d += 1;
    }
    d
}

// Byte length of one multipart/byteranges part for (first, last). Every
// literal length comes from the same consts the writer uses.
pub(crate) fn part_len(first: u64, last: u64, total: u64) -> u64 {
    let b = RANGE_BOUNDARY.len() as u64;
    (2 + b + 2) // --boundary\r\n
        + PART_CT.len() as u64
        + (PART_CR_PRE.len() as u64 + digits_u64(first) + 1 + digits_u64(last) + 1 + digits_u64(total) + 2)
        + 2 // blank line
        + (last - first + 1) // body
        + 2 // \r\n
}

pub(crate) fn range_head_start(h: &mut Out, status: u16) {
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

pub(crate) fn range_head_end(h: &mut Out, content_length: u64, keep_alive: bool) {
    h.push_str("Content-Length: ");
    h.push_u64(content_length);
    h.push_str("\r\nConnection: ");
    h.push_str(if keep_alive { "keep-alive" } else { "close" });
    h.push_str("\r\n\r\n");
}

pub(crate) fn send_range_full(stream: &mut TcpStream, keep_alive: bool, with_body: bool, etag: &str) -> bool {
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

pub(crate) fn send_gzip_full(stream: &mut TcpStream, keep_alive: bool, with_body: bool) -> bool {
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

pub(crate) fn send_range_single(
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

pub(crate) fn send_range_multi(
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

pub(crate) fn send_not_modified(stream: &mut TcpStream, keep_alive: bool, etag: &str) -> bool {
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
pub(crate) fn etag_list_matches(value: &str, etag: &str) -> bool {
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

pub(crate) fn send_ranges(stream: &mut TcpStream, req: &Request, keep_alive: bool) -> bool {
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

pub(crate) const RANGE_ETAG_GZIP: &str = "\"zahttp-bytes-v1+gzip\"";

// true when the client will take a gzip content-coding (RFC 7231 5.3.4).
// Comma-separated tokens; `gzip;q=0` opts out, `*` (with q>0) counts.
pub(crate) fn q_is_zero(s: &[u8]) -> bool {
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

pub(crate) fn qvalue_zero(params: &[u8]) -> bool {
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

pub(crate) fn accepts_gzip(req: &Request) -> bool {
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
