//! Go duration strings (`"2s"`, `"5m"`, `"1h30m"`, `"1.5h"`, `"300ms"`), so
//! config files written for the Go version keep parsing exactly as before
//! (ADR-0005), and Go's `Duration.String()` for log lines.

use std::fmt;
use std::time::Duration;

const NS: u128 = 1;
const US: u128 = 1_000 * NS;
const MS: u128 = 1_000 * US;
const SEC: u128 = 1_000 * MS;
const MIN: u128 = 60 * SEC;
const HOUR: u128 = 60 * MIN;

/// Go's limit: a duration is an int64 count of nanoseconds.
const MAX_NANOS: u128 = 1 << 63;

/// Parses a Go duration string the way `time.ParseDuration` does. Returns the
/// signed number of nanoseconds; callers decide whether negative is allowed.
pub fn parse(orig: &str) -> Result<i128, String> {
    let invalid = || format!("time: invalid duration \"{orig}\"");
    let mut s = orig;
    let mut neg = false;
    if let Some(c) = s.chars().next()
        && (c == '-' || c == '+')
    {
        neg = c == '-';
        s = &s[1..];
    }
    if s == "0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }

    let mut total: u128 = 0;
    while !s.is_empty() {
        let first = s.as_bytes()[0];
        if !(first == b'.' || first.is_ascii_digit()) {
            return Err(invalid());
        }

        // Integer part.
        let int_len = s.bytes().take_while(u8::is_ascii_digit).count();
        let pre = int_len > 0;
        let mut v: u128 = 0;
        for b in s[..int_len].bytes() {
            v = v * 10 + u128::from(b - b'0');
            if v > MAX_NANOS {
                return Err(invalid());
            }
        }
        s = &s[int_len..];

        // Fraction part: digits past what fits are consumed but ignored, as in Go.
        let mut f: u128 = 0;
        let mut scale: u128 = 1;
        let mut post = false;
        if let Some(rest) = s.strip_prefix('.') {
            let frac_len = rest.bytes().take_while(u8::is_ascii_digit).count();
            post = frac_len > 0;
            let mut overflow = false;
            for b in rest[..frac_len].bytes() {
                if overflow || f > (MAX_NANOS - 1) / 10 {
                    overflow = true;
                    continue;
                }
                f = f * 10 + u128::from(b - b'0');
                scale *= 10;
            }
            s = &rest[frac_len..];
        }
        if !pre && !post {
            return Err(invalid());
        }

        // Unit.
        let unit_len = s
            .bytes()
            .take_while(|b| *b != b'.' && !b.is_ascii_digit())
            .count();
        if unit_len == 0 {
            return Err(format!("time: missing unit in duration \"{orig}\""));
        }
        let u = &s[..unit_len];
        s = &s[unit_len..];
        let unit = match u {
            "ns" => NS,
            "us" | "\u{b5}s" | "\u{3bc}s" => US,
            "ms" => MS,
            "s" => SEC,
            "m" => MIN,
            "h" => HOUR,
            _ => return Err(format!("time: unknown unit \"{u}\" in duration \"{orig}\"")),
        };
        if v > MAX_NANOS / unit {
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            v += f * unit / scale;
            if v > MAX_NANOS {
                return Err(invalid());
            }
        }
        total += v;
        if total > MAX_NANOS {
            return Err(invalid());
        }
    }
    if neg {
        return Ok(-(total as i128));
    }
    if total > MAX_NANOS - 1 {
        return Err(invalid());
    }
    Ok(total as i128)
}

/// Displays a [`Duration`] the way Go's `Duration.String()` does:
/// `0s`, `300ms`, `2s`, `1m0s`, `1h30m0s`.
#[derive(Debug, Clone, Copy)]
pub struct GoDuration(pub Duration);

impl fmt::Display for GoDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let u = self.0.as_nanos();
        if u == 0 {
            return f.write_str("0s");
        }
        if u < SEC {
            let (unit, prec) = if u < US {
                ("ns", 0)
            } else if u < MS {
                ("\u{b5}s", 3)
            } else {
                ("ms", 6)
            };
            let (int, frac) = split_frac(u, prec);
            return write!(f, "{int}{frac}{unit}");
        }
        let (secs, frac) = split_frac(u, 9);
        let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
        if h > 0 {
            write!(f, "{h}h{m}m{s}{frac}s")
        } else if secs >= 60 {
            write!(f, "{m}m{s}{frac}s")
        } else {
            write!(f, "{s}{frac}s")
        }
    }
}

/// Splits `v` into `v / 10^prec` and a `.ddd` fraction with trailing zeros
/// (and the point, if nothing is left) removed.
fn split_frac(v: u128, prec: u32) -> (u128, String) {
    let p = 10u128.pow(prec);
    let (int, rem) = (v / p, v % p);
    if rem == 0 {
        return (int, String::new());
    }
    let digits = format!("{rem:0width$}", width = prec as usize);
    (int, format!(".{}", digits.trim_end_matches('0')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> i128 {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_go_durations() {
        let s = SEC as i128;
        assert_eq!(ok("0"), 0);
        assert_eq!(ok("-0"), 0);
        assert_eq!(ok("0s"), 0);
        assert_eq!(ok("2s"), 2 * s);
        assert_eq!(ok("+5s"), 5 * s);
        assert_eq!(ok("-1s"), -s);
        assert_eq!(ok("5m"), 300 * s);
        assert_eq!(ok("60s"), 60 * s);
        assert_eq!(ok("1h30m"), 5400 * s);
        assert_eq!(ok("1.5h"), 5400 * s);
        assert_eq!(ok("300ms"), 300 * MS as i128);
        assert_eq!(ok("1h2m3s4ms5us6ns"), 3_723_004_005_006);
        assert_eq!(ok("3\u{b5}s"), 3_000);
        assert_eq!(ok("3\u{3bc}s"), 3_000);
        assert_eq!(ok(".5s"), s / 2);
        assert_eq!(ok("5.s"), 5 * s);
        assert_eq!(ok("1.0000000001s"), s);
        assert_eq!(ok("2562047h47m16.854775807s"), i64::MAX as i128);
    }

    #[test]
    fn rejects_what_go_rejects() {
        for bad in [
            "",
            "s",
            "-",
            "soon",
            ".s",
            "1",
            "1d",
            "3.5",
            "1h-1m",
            "2562047h47m16.854775808s",
            " 2s",
            "2s ",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(
            parse("1").unwrap_err(),
            "time: missing unit in duration \"1\""
        );
        assert_eq!(
            parse("1d").unwrap_err(),
            "time: unknown unit \"d\" in duration \"1d\""
        );
        assert_eq!(
            parse("soon").unwrap_err(),
            "time: invalid duration \"soon\""
        );
    }

    #[test]
    fn displays_like_go() {
        let d = |s: &str| GoDuration(Duration::from_nanos(ok(s) as u64)).to_string();
        assert_eq!(d("0s"), "0s");
        assert_eq!(d("2s"), "2s");
        assert_eq!(d("60s"), "1m0s");
        assert_eq!(d("5m"), "5m0s");
        assert_eq!(d("10m"), "10m0s");
        assert_eq!(d("1.5h"), "1h30m0s");
        assert_eq!(d("300ms"), "300ms");
        assert_eq!(d("1.5s"), "1.5s");
        assert_eq!(d("1500us"), "1.5ms");
        assert_eq!(d("1500ns"), "1.5\u{b5}s");
        assert_eq!(d("7ns"), "7ns");
        assert_eq!(d("1h0m0.25s"), "1h0m0.25s");
    }
}
