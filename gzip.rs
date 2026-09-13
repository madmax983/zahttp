// zahttp module: gzip (hand-rolled DEFLATE) — zero deps, zero heap. See main.rs for the rules.

// ---- gzip content-coding (RFC 1952): the unit ---------------------------
// Hand-rolled DEFLATE with fixed Huffman codes (RFC 1951 3.2.6): LZ77 with
// 3-byte hash chains over a 32 KiB window, greedy matches, an LSB-first
// bit writer, and a table-free CRC32. Everything over fixed stack buffers;
// the 64 KiB body is compressed once into a LazyLock and every response
// borrows slices of it.

// worst case: 64 KiB of literals at 9 bits each + framing
pub(crate) const GZIP_CAP: usize = 74752;
pub(crate) const GZIP_MAX_IN: usize = 65536;

pub(crate) struct BitW<'a> {
    pub(crate) out: &'a mut [u8],
    pub(crate) pos: usize,
    pub(crate) acc: u32,   // pending bits, LSB-first
    pub(crate) nbits: u32, // how many are pending
    pub(crate) ok: bool,
}

impl<'a> BitW<'a> {
    pub(crate) fn bits(&mut self, value: u32, count: u32) {
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
    pub(crate) fn flush(&mut self) {
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

pub(crate) fn rev_bits(mut v: u32, mut n: u32) -> u32 {
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
pub(crate) fn lit_code(sym: u32) -> (u32, u32) {
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

pub(crate) const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83,
    99, 115, 131, 163, 195, 227, 258,
];
pub(crate) const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5,
    5, 5, 0,
];
pub(crate) const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769,
    1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
pub(crate) const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11,
    11, 12, 12, 13, 13,
];

// -> (symbol, extra bits, extra value)
pub(crate) fn len_code(len: u32) -> (u32, u32, u32) {
    let mut i = 0usize;
    while i + 1 < LEN_BASE.len() && (LEN_BASE[i + 1] as u32) <= len {
        i += 1;
    }
    (257 + i as u32, LEN_EXTRA[i] as u32, len - LEN_BASE[i] as u32)
}

pub(crate) fn dist_code(dist: u32) -> (u32, u32, u32) {
    let mut i = 0usize;
    while i + 1 < DIST_BASE.len() && (DIST_BASE[i + 1] as u32) <= dist {
        i += 1;
    }
    (i as u32, DIST_EXTRA[i] as u32, dist - DIST_BASE[i] as u32)
}

pub(crate) fn crc32(data: &[u8]) -> u32 {
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

pub(crate) fn emit_lit(w: &mut BitW, sym: u32) {
    let (c, n) = lit_code(sym);
    w.bits(rev_bits(c, n), n);
}

pub(crate) fn hash3(b0: u8, b1: u8, b2: u8) -> usize {
    (((b0 as u32) << 10) ^ ((b1 as u32) << 5) ^ (b2 as u32)) as usize & 8191
}

pub(crate) fn gzip_encode(input: &[u8], out: &mut [u8]) -> Option<usize> {
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


