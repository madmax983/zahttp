// zahttp module: buf (fixed buffers) — zero deps, zero heap. See main.rs for the rules.

use std::io::{IoSlice, Write};
use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

// ---- fixed capacities: the whole server lives in these ------------------

pub(crate) const READ_CAP: usize = 8192; // largest request head we will buffer
pub(crate) const BODY_CAP: usize = 4096; // largest request body we accept
pub(crate) const HDR_MAX: usize = 32; // most headers per request
pub(crate) const RESP_HEAD_CAP: usize = 512;
pub(crate) const RESP_BODY_CAP: usize = 4096;

// ---- Out: a tiny non-allocating byte writer over a fixed buffer ---------

pub(crate) struct Out<'b> {
    pub(crate) buf: &'b mut [u8],
    pub(crate) len: usize,
    pub(crate) overflow: bool,
}

impl<'b> Out<'b> {
    pub(crate) fn new(buf: &'b mut [u8]) -> Out<'b> {
        Out { buf, len: 0, overflow: false }
    }
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        let space = self.buf.len().saturating_sub(self.len);
        let take = bytes.len().min(space);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        if take < bytes.len() {
            self.overflow = true;
        }
    }
    pub(crate) fn push_str(&mut self, s: &str) {
        self.push(s.as_bytes());
    }
    pub(crate) fn push_u64(&mut self, mut v: u64) {
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
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

pub(crate) fn push2(o: &mut Out, v: u64) {
    o.push(&[b'0' + (v / 10) as u8, b'0' + (v % 10) as u8]);
}

pub(crate) fn push4(o: &mut Out, v: u64) {
    o.push(&[
        b'0' + (v / 1000) as u8,
        b'0' + ((v / 100) % 10) as u8,
        b'0' + ((v / 10) % 10) as u8,
        b'0' + (v % 10) as u8,
    ]);
}

pub(crate) const HEX: &[u8; 16] = b"0123456789abcdef";

pub(crate) fn push_hex_u64(o: &mut Out, v: u64) {
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

pub(crate) fn push_hex_usize(o: &mut Out, v: usize) {
    push_hex_u64(o, v as u64);
}

// ---- HTTP date, computed by hand from the unix clock --------------------

pub(crate) const DAYS: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
pub(crate) const MONTHS: [&[u8; 3]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

pub(crate) fn http_date_unix(secs: u64, out: &mut [u8; 29]) {
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

pub(crate) fn date_now(out: &mut [u8; 29]) {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => http_date_unix(d.as_secs(), out),
        Err(_) => {
            let mut o = Out::new(&mut out[..]);
            o.push(b"Thu, 01 Jan 1970 00:00:00 GMT");
        }
    }
}

pub(crate) fn parse_u64b(s: &[u8]) -> Option<u64> {
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

// ---- write2: one vectored write for head+body ---------------------------
// Performance contract: every non-streaming response emits its headers
// and body in a single write_vectored() call. A tiny header followed by
// a tiny body used to leave as two separate write() calls; with Nagle
// enabled the second, small segment waited on the delayed ACK of the
// first (~40 ms on loopback), which is why sequential /health showed a
// 41 ms p50 while the head arrived in under 1 ms. One writev means one
// segment, so the pair can never be split across the ACK boundary again.
//
// Partial writes are handled by looping and advancing across both
// slices — a vectored write is never assumed to take everything. The
// IoSlice pair lives on the stack: zero allocation, like everything
// else here. Ok(0) on a non-empty write means the peer is gone; we
// report failure instead of spinning.
pub(crate) fn write2(stream: &mut TcpStream, a: &[u8], b: &[u8]) -> bool {
    let mut a = a;
    let mut b = b;
    loop {
        if a.is_empty() && b.is_empty() {
            return true;
        }
        let bufs = [IoSlice::new(a), IoSlice::new(b)];
        let from = usize::from(a.is_empty());
        match stream.write_vectored(&bufs[from..]) {
            Ok(0) => return false,
            Ok(n) => {
                let mut rem = n;
                let take = rem.min(a.len());
                a = &a[take..];
                rem -= take;
                b = &b[rem.min(b.len())..];
            }
            Err(_) => return false,
        }
    }
}

// Parse one byte-range-spec into an inclusive (first, last). False when
// malformed or unsatisfiable against `total`.
