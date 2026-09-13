// zahttp module: ws (RFC 6455) — zero deps, zero heap. See main.rs for the rules.

use std::io::{Read, Write};
use std::net::TcpStream;

use crate::buf::{date_now, write2, Out};
use crate::http::{header, trim, Request};

// ---- SHA-1 (FIPS 180-4), hand-rolled ------------------------------------

pub(crate) fn sha1_block(block: &[u8], h: &mut [u32; 5]) {
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

pub(crate) fn sha1(msg: &[u8], out: &mut [u8; 20]) {
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

pub(crate) const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

// Encodes into `out` (must fit ((len+2)/3)*4 bytes); returns encoded length.
pub(crate) fn base64_encode(input: &[u8], out: &mut [u8]) -> usize {
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

// ---- WebSocket (RFC 6455): handshake + frame codec, zero heap -----------

pub(crate) const WS_MSG_CAP: usize = 4096;
pub(crate) const WS_GUID: &[u8; 36] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// True when any header named `name` carries `token` in a comma-separated
// list. Case-insensitive, per the HTTP token rules.
pub(crate) fn header_has_token(req: &Request, name: &str, token: &[u8]) -> bool {
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

pub(crate) fn ws_reject(stream: &mut TcpStream, status: u16, reason: &str, version_header: bool) {
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
pub(crate) fn read_full(stream: &mut TcpStream, buf: &mut [u8]) -> bool {
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
pub(crate) fn ws_drain(stream: &mut TcpStream, len: u64) -> bool {
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
pub(crate) fn ws_send(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> bool {
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
    // One writev for frame head+payload: same Nagle reasoning as
    // buf::write2 — a tiny echo frame must not wait on a delayed ACK.
    write2(stream, &hbuf[..hlen], payload)
}

pub(crate) fn ws_close(stream: &mut TcpStream, code: u16, reason: &[u8]) {
    let mut pbuf = [0u8; 125];
    pbuf[0] = (code >> 8) as u8;
    pbuf[1] = code as u8;
    let rlen = reason.len().min(123);
    pbuf[2..2 + rlen].copy_from_slice(&reason[..rlen]);
    let _ = ws_send(stream, 0x8, &pbuf[..2 + rlen]);
}

pub(crate) fn ws_serve(stream: &mut TcpStream, req: &Request) {
    // An upgraded connection is no longer HTTP keep-alive, so the HTTP
    // idle read timeout must not reap a quiet websocket.
    let _ = stream.set_read_timeout(None);
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

pub(crate) fn ws_loop(stream: &mut TcpStream) {
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
