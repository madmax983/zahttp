// zahttp module: dates — HTTP-date parsing (RFC 7231 7.1.1.1).
// Zero deps, zero heap. See main.rs for the rules.
//
// Contract: parse_http_date() turns an HTTP-date header value into unix
// seconds, accepting all three formats the RFC requires recipients to
// accept:
//   IMF-fixdate  "Sun, 06 Nov 1994 08:49:37 GMT"      (the only format we send)
//   RFC 850      "Sunday, 06-Nov-94 08:49:37 GMT"     (obsolete, still legal)
//   asctime      "Sun Nov  6 08:49:37 1994"            (obsolete, still legal)
// Anything malformed — wrong widths, unknown month, impossible day
// (2021-02-29), out-of-range time — is None. Callers treat an
// unparsable conditional date as absent, per RFC 7232 ("MUST ignore the
// header field" when the date is invalid).
//
// Two deliberate leniencies, documented not hidden:
//   * the weekday name is checked for shape (3 alpha / full name) but
//     never cross-checked against the calendar — weekday math adds
//     nothing to precondition comparisons;
//   * a seconds value of 60 (leap second) is accepted and folds into
//     the next minute, rather than failing the whole date.
// Two-digit RFC 850 years follow the RFC's pivot rule: values more than
// 50 years in the future are read as the same digits in the past
// century ("94" -> 1994, "26" -> 2026 while the current year is 2026).

use std::time::{SystemTime, UNIX_EPOCH};

use crate::buf::http_date_unix;
use crate::http::trim;

fn two(d: &[u8]) -> Option<u64> {
    if d.len() != 2 || !d[0].is_ascii_digit() || !d[1].is_ascii_digit() {
        return None;
    }
    Some((d[0] - b'0') as u64 * 10 + (d[1] - b'0') as u64)
}

fn four(d: &[u8]) -> Option<u64> {
    if d.len() != 4 {
        return None;
    }
    let mut v = 0u64;
    for &c in d {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u64;
    }
    Some(v)
}

// asctime pads a single-digit day with a space ("Nov  6").
fn day2(d: &[u8]) -> Option<u64> {
    if d.len() != 2 {
        return None;
    }
    let tens = match d[0] {
        b' ' => 0,
        c if c.is_ascii_digit() => (c - b'0') as u64,
        _ => return None,
    };
    let ones = match d[1] {
        c if c.is_ascii_digit() => (c - b'0') as u64,
        _ => return None,
    };
    let v = tens * 10 + ones;
    if v == 0 {
        return None;
    }
    Some(v)
}

fn month(m: &[u8]) -> Option<u64> {
    Some(match m {
        b"Jan" => 1,
        b"Feb" => 2,
        b"Mar" => 3,
        b"Apr" => 4,
        b"May" => 5,
        b"Jun" => 6,
        b"Jul" => 7,
        b"Aug" => 8,
        b"Sep" => 9,
        b"Oct" => 10,
        b"Nov" => 11,
        b"Dec" => 12,
        _ => return None,
    })
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: u64) -> Option<u64> {
    Some(match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => return None,
    })
}

// Inverse of buf::http_date_unix: Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: u64, d: u64) -> Option<i64> {
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m)? {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = m as i64 + if m > 2 { -3 } else { 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

fn combine(y: u64, m: u64, d: u64, hh: u64, mm: u64, ss: u64) -> Option<u64> {
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y as i64, m, d)?;
    if days < 0 {
        return None; // before the epoch: not a date we can serve
    }
    let tod = hh * 3600 + mm * 60 + ss;
    (days as u64).checked_mul(86_400)?.checked_add(tod)
}

fn current_year() -> u64 {
    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return 1970,
    };
    let mut out = [0u8; 29];
    http_date_unix(secs, &mut out);
    match four(&out[12..16]) {
        Some(y) => y,
        None => 1970,
    }
}

// RFC 7231 7.1.1.1: a two-digit year more than 50 years in the future
// is the most recent past year with those digits.
fn pivot_2digit(yy: u64) -> u64 {
    let y = 2000 + yy;
    if y > current_year() + 50 {
        y - 100
    } else {
        y
    }
}

fn alpha3(s: &[u8]) -> bool {
    s.len() == 3 && s.iter().all(|c| c.is_ascii_alphabetic())
}

// "Sun, 06 Nov 1994 08:49:37 GMT" — exactly 29 bytes.
fn parse_imf(s: &[u8]) -> Option<u64> {
    if s.len() != 29
        || !alpha3(&s[0..3])
        || s[3] != b','
        || s[4] != b' '
        || s[7] != b' '
        || s[11] != b' '
        || s[16] != b' '
        || s[19] != b':'
        || s[22] != b':'
        || s[25] != b' '
        || &s[26..29] != b"GMT"
    {
        return None;
    }
    combine(
        four(&s[12..16])?,
        month(&s[8..11])?,
        two(&s[5..7])?,
        two(&s[17..19])?,
        two(&s[20..22])?,
        two(&s[23..25])?,
    )
}

// "Sunday, 06-Nov-94 08:49:37 GMT" — full weekday name, two-digit year.
fn parse_rfc850(s: &[u8]) -> Option<u64> {
    let comma = s.iter().position(|&c| c == b',')?;
    if comma < 6 || comma > 9 || s.len() != comma + 24 {
        return None;
    }
    if !s[..comma].iter().all(|c| c.is_ascii_alphabetic()) || s[comma + 1] != b' ' {
        return None;
    }
    let o = comma;
    if s[o + 4] != b'-' || s[o + 8] != b'-' || s[o + 11] != b' ' || s[o + 14] != b':' || s[o + 17] != b':' || s[o + 20] != b' ' || &s[o + 21..o + 24] != b"GMT" {
        return None;
    }
    combine(
        pivot_2digit(two(&s[o + 9..o + 11])?),
        month(&s[o + 5..o + 8])?,
        two(&s[o + 2..o + 4])?,
        two(&s[o + 12..o + 14])?,
        two(&s[o + 15..o + 17])?,
        two(&s[o + 18..o + 20])?,
    )
}

// "Sun Nov  6 08:49:37 1994" — 24 bytes, space-padded day.
fn parse_asctime(s: &[u8]) -> Option<u64> {
    if s.len() != 24
        || !alpha3(&s[0..3])
        || s[3] != b' '
        || s[7] != b' '
        || s[10] != b' '
        || s[13] != b':'
        || s[16] != b':'
        || s[19] != b' '
    {
        return None;
    }
    combine(
        four(&s[20..24])?,
        month(&s[4..7])?,
        day2(&s[8..10])?,
        two(&s[11..13])?,
        two(&s[14..16])?,
        two(&s[17..19])?,
    )
}

pub(crate) fn parse_http_date(value: &[u8]) -> Option<u64> {
    let s = trim(value);
    if s.is_empty() {
        return None;
    }
    if let Some(t) = parse_imf(s) {
        return Some(t);
    }
    if let Some(t) = parse_rfc850(s) {
        return Some(t);
    }
    parse_asctime(s)
}
