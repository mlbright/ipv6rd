//! Log lines on stderr in the shape Go's `log/slog` text handler wrote them:
//! `time=… level=INFO msg="…" key=value …`, so existing journal searches and
//! the README's troubleshooting lines keep working (ADR-0005). Times are UTC.

use std::fmt::{self, Display, Write as _};
use std::net::Ipv6Addr;
use std::time::{SystemTime, UNIX_EPOCH};

/// Log a line at INFO: `log_info!("msg", "key" => value, ...)`.
#[macro_export]
macro_rules! log_info {
    ($msg:expr $(, $k:literal => $v:expr)* $(,)?) => {
        $crate::log::emit($crate::log::Level::Info, $msg,
            &[$(($k, &$v as &dyn ::std::fmt::Display)),*])
    };
}

/// Log a line at WARN.
#[macro_export]
macro_rules! log_warn {
    ($msg:expr $(, $k:literal => $v:expr)* $(,)?) => {
        $crate::log::emit($crate::log::Level::Warn, $msg,
            &[$(($k, &$v as &dyn ::std::fmt::Display)),*])
    };
}

/// Log a line at ERROR.
#[macro_export]
macro_rules! log_error {
    ($msg:expr $(, $k:literal => $v:expr)* $(,)?) => {
        $crate::log::emit($crate::log::Level::Error, $msg,
            &[$(($k, &$v as &dyn ::std::fmt::Display)),*])
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// Writes one log line to stderr. `eprint!` (not a raw stderr handle) so test
/// output capture works.
pub fn emit(level: Level, msg: &str, attrs: &[(&str, &dyn Display)]) {
    eprint!("{}", format_line(SystemTime::now(), level, msg, attrs));
}

/// Formats one complete line, including the trailing newline.
pub fn format_line(
    now: SystemTime,
    level: Level,
    msg: &str,
    attrs: &[(&str, &dyn Display)],
) -> String {
    let mut out = String::with_capacity(160);
    out.push_str("time=");
    out.push_str(&rfc3339_millis_utc(now));
    out.push_str(" level=");
    out.push_str(level.as_str());
    out.push_str(" msg=");
    push_value(&mut out, msg);
    for (k, v) in attrs {
        out.push(' ');
        push_value(&mut out, k);
        out.push('=');
        push_value(&mut out, &v.to_string());
    }
    out.push('\n');
    out
}

/// Appends `s`, quoted the way slog's text handler quotes: only when it is
/// empty or contains a space, `=`, `"`, or a control or non-printable char.
fn push_value(out: &mut String, s: &str) {
    if needs_quoting(s) {
        out.push_str(&go_quote(s));
    } else {
        out.push_str(s);
    }
}

fn needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.chars().any(|c| {
            if c.is_ascii() {
                c == ' ' || c == '=' || c == '"' || c.is_ascii_control()
            } else {
                c.is_whitespace() || c.is_control()
            }
        })
}

/// A double-quoted string with Go `strconv.Quote` escapes.
fn go_quote(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            '\n' => q.push_str("\\n"),
            '\r' => q.push_str("\\r"),
            '\t' => q.push_str("\\t"),
            '\x07' => q.push_str("\\a"),
            '\x08' => q.push_str("\\b"),
            '\x0c' => q.push_str("\\f"),
            '\x0b' => q.push_str("\\v"),
            c if c.is_ascii_control() => {
                let _ = write!(q, "\\x{:02x}", c as u32);
            }
            c if c.is_control() => {
                let _ = write!(q, "\\u{:04x}", c as u32);
            }
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

/// `2006-01-02T15:04:05.000Z` for `t` in UTC.
fn rfc3339_millis_utc(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, m, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Formats a list the way Go's `%v` prints a `[]string`: `[a b c]`.
#[derive(Debug)]
pub struct List<'a, T>(pub &'a [T]);

impl<T: Display> Display for List<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('[')?;
        for (i, v) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            write!(f, "{v}")?;
        }
        f.write_char(']')
    }
}

/// An optional address, printed as `(none)` when absent.
#[derive(Debug)]
pub struct OrNone(pub Option<Ipv6Addr>);

impl Display for OrNone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(a) => write!(f, "{a}"),
            None => f.write_str("(none)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn line_shape_matches_slog_text() {
        let t = UNIX_EPOCH + Duration::from_millis(1_764_003_960_123);
        let access = ["tcp/443".to_string(), "tcp/22".to_string()];
        let line = format_line(
            t,
            Level::Info,
            "egress address changed",
            &[
                ("reason", &"startup"),
                ("access", &List(&access)),
                ("previous", &OrNone(None)),
            ],
        );
        assert_eq!(
            line,
            "time=2025-11-24T17:06:00.123Z level=INFO msg=\"egress address changed\" \
             reason=startup access=\"[tcp/443 tcp/22]\" previous=(none)\n"
        );
    }

    #[test]
    fn quoting_follows_slog() {
        let q = |s: &str| {
            let mut o = String::new();
            push_value(&mut o, s);
            o
        };
        assert_eq!(q("plain"), "plain");
        assert_eq!(q("op://vault/item/field"), "op://vault/item/field");
        assert_eq!(q(""), "\"\"");
        assert_eq!(q("a=b"), "\"a=b\"");
        assert_eq!(q("two words"), "\"two words\"");
        assert_eq!(q("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(q("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(q("back\\slash"), "back\\slash");
        assert_eq!(q("bell\x07"), "\"bell\\a\"");
        assert_eq!(q("nul\x00"), "\"nul\\x00\"");
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_781), (2026, 11, 24));
    }
}
