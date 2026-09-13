// zahttp module: multipart (RFC 7578) — zero deps, zero heap. See main.rs for the rules.

use crate::buf::{Out};
use crate::http::{header, trim, Request};

// ---- multipart/form-data parsing (RFC 7578): POST /upload --------------
// Parses the request body in place. Zero heap: part descriptors live in a
// fixed array and every string borrows from the body buffer. Documented
// simplifications: no backslash-escape processing inside quoted values,
// naive boundary search (a boundary chosen to collide with content will
// confuse any simple parser).

pub(crate) const MAX_PARTS: usize = 16;
pub(crate) const MAX_BOUNDARY: usize = 128;

#[derive(Clone, Copy)]
pub(crate) struct FormPart<'b> {
    pub(crate) name: &'b [u8],
    pub(crate) filename: Option<&'b [u8]>,
    pub(crate) content_type: &'b [u8],
    pub(crate) body: &'b [u8],
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
pub(crate) fn memfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// Does `needle` occur at `body[pos..]`? Bounds-safe.
pub(crate) fn at(body: &[u8], pos: usize, needle: &[u8]) -> bool {
    body.len() >= pos + needle.len() && &body[pos..pos + needle.len()] == needle
}

// Find a `key=value` (or `key="quoted value"`) parameter in a `;`-separated
// header value. The key must start at a parameter boundary so `name` never
// matches inside `filename`.
pub(crate) fn param<'b>(hv: &'b [u8], key: &[u8]) -> Option<&'b [u8]> {
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

pub(crate) fn parse_part_headers<'b>(head: &'b [u8]) -> (&'b [u8], Option<&'b [u8]>, &'b [u8]) {
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

pub(crate) fn multipart_boundary<'a>(req: &'a Request<'a>) -> Option<&'a [u8]> {
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
pub(crate) fn parse_multipart<'b>(
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

pub(crate) fn serve_upload(req: &Request, out: &mut Out) -> Result<(), u16> {
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
