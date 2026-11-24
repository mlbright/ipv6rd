//! The TOML configuration file. Same keys, defaults and validation as the Go
//! version (ADR-0003, ADR-0005), plus the optional `op_path` (ADR-0007).

use std::net::{IpAddr, Ipv6Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fkospa::AccessSpec;
use serde::Deserialize;

use crate::duration;

/// The validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Quiet period after an address event before recomputing.
    pub debounce: Duration,
    /// Reconciliation poll interval (backstop for missed events).
    pub reconcile: Duration,
    /// Grant refresh interval; `None` disables it.
    pub refresh: Option<Duration>,
    /// Per-attempt knock timeout.
    pub timeout: Duration,
    /// Maximum knock attempts before giving up.
    pub attempts: u32,
    /// Explicit path to the `op` binary; `None` looks it up on `PATH`.
    pub op_path: Option<PathBuf>,
    pub fwknop: FwknopConfig,
}

/// The SPA settings. The keys themselves are not here; they are fetched from
/// 1Password at startup.
#[derive(Debug, Clone)]
pub struct FwknopConfig {
    /// The SPA target as configured (hostname or IPv6 literal).
    pub dest: String,
    /// The SPA target's single IPv6 address, looked up once at startup. It is
    /// both the probe destination and where every SPA packet goes (ADR-0006).
    pub target: Ipv6Addr,
    /// UDP port fwknopd listens on.
    pub port: u16,
    /// The access request: one entry per SPA packet.
    pub access: Vec<AccessSpec>,
    /// 1Password secret reference for the base64 encryption key.
    pub key_ref: String,
    /// 1Password secret reference for the base64 HMAC key.
    pub hmac_ref: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    debounce: Option<String>,
    reconcile: Option<String>,
    refresh: Option<String>,
    timeout: Option<String>,
    retries: Option<i64>,
    op_path: Option<String>,
    fwknop: Option<FwknopFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FwknopFile {
    #[serde(default)]
    dest: String,
    #[serde(default)]
    port: i64,
    /// `None` (absent) defaults to `["tcp/443"]`; an explicit `[]` is an error.
    access: Option<Vec<String>>,
    #[serde(default)]
    key_ref: String,
    #[serde(default)]
    hmac_ref: String,
}

/// Reads, strictly decodes and validates the config at `path`. A missing file
/// or any unknown key is an error.
pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("reading config {}: {e}", path.display()))?;
    parse(&text, path, resolve_dest)
}

/// Parses config text; `resolve` turns `fwknop.dest` into an IPv6 address
/// (injectable so tests need no DNS).
pub fn parse(
    text: &str,
    path: &Path,
    resolve: impl Fn(&str) -> Result<Ipv6Addr, String>,
) -> Result<Config, String> {
    let fc: FileConfig = toml::from_str(text).map_err(|e| {
        format!(
            "reading config {}: {}",
            path.display(),
            e.to_string().trim_end()
        )
    })?;

    let debounce = parse_duration(
        "debounce",
        fc.debounce.as_deref(),
        Duration::from_secs(2),
        true,
    )?;
    let reconcile = parse_duration(
        "reconcile",
        fc.reconcile.as_deref(),
        Duration::from_secs(300),
        false,
    )?;
    let refresh = parse_duration("refresh", fc.refresh.as_deref(), Duration::ZERO, true)?;
    let timeout = parse_duration(
        "timeout",
        fc.timeout.as_deref(),
        Duration::from_secs(60),
        false,
    )?;

    let attempts = fc.retries.unwrap_or(3);
    if attempts < 1 {
        return Err("retries must be >= 1".into());
    }
    let attempts = u32::try_from(attempts).map_err(|_| "retries is too large".to_string())?;

    let op_path = fc
        .op_path
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from);

    let f = fc.fwknop.ok_or("missing [fwknop] section")?;
    if f.dest.is_empty() {
        return Err("fwknop.dest is required (the SPA target host)".into());
    }
    if !(1..=65535).contains(&f.port) {
        return Err("fwknop.port must be in 1..65535".into());
    }
    if f.key_ref.is_empty() {
        return Err("fwknop.key_ref is required".into());
    }
    if f.hmac_ref.is_empty() {
        return Err("fwknop.hmac_ref is required".into());
    }

    let entries = match f.access {
        None => vec!["tcp/443".to_string()],
        Some(v) if v.is_empty() => {
            return Err("fwknop.access is empty; list at least one protocol/port \
                        (e.g. \"tcp/443\") or omit it for the default"
                .into());
        }
        Some(v) => v,
    };
    let access = entries
        .iter()
        .map(|e| parse_access_entry(e).map_err(|err| format!("fwknop.access entry \"{e}\": {err}")))
        .collect::<Result<Vec<_>, _>>()?;

    let target = resolve(&f.dest)?;

    Ok(Config {
        debounce,
        reconcile,
        refresh: (!refresh.is_zero()).then_some(refresh),
        timeout,
        attempts,
        op_path,
        fwknop: FwknopConfig {
            dest: f.dest,
            target,
            port: f.port as u16,
            access,
            key_ref: f.key_ref,
            hmac_ref: f.hmac_ref,
        },
    })
}

/// Parses a Go duration string, returning `def` when absent or blank. Unless
/// `allow_zero`, the result must be strictly positive.
fn parse_duration(
    name: &str,
    val: Option<&str>,
    def: Duration,
    allow_zero: bool,
) -> Result<Duration, String> {
    let Some(val) = val.filter(|v| !v.trim().is_empty()) else {
        return Ok(def);
    };
    let nanos = duration::parse(val).map_err(|e| format!("{name}: {e}"))?;
    if nanos < 0 || (nanos == 0 && !allow_zero) {
        return Err(format!("{name} must be positive"));
    }
    Ok(Duration::from_nanos(nanos as u64))
}

/// One access-request entry: `tcp/PORT` or `udp/PORT`. Validated with the Go
/// version's rules, then parsed with fkospa's own parser — the one the
/// fwknop-rs server decodes with — so nothing the server would refuse passes.
fn parse_access_entry(e: &str) -> Result<AccessSpec, String> {
    if e.contains([',', ' ']) {
        return Err("one protocol/port per entry; do not put several in one string".into());
    }
    let (proto, port) = e
        .split_once('/')
        .ok_or("expected \"proto/port\", e.g. tcp/443")?;
    if proto != "tcp" && proto != "udp" {
        return Err(format!("protocol \"{proto}\" must be tcp or udp"));
    }
    if port.contains('-') {
        return Err(
            "port ranges are not supported: an SPA message carries a single port, \
                    so list each port as its own entry"
                .into(),
        );
    }
    parse_port(port)?;
    e.parse::<AccessSpec>().map_err(|err| err.to_string())
}

fn parse_port(s: &str) -> Result<u16, String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("port \"{s}\" is not a number"));
    }
    match s.parse::<u32>() {
        Ok(n @ 1..=65535) => Ok(n as u16),
        Ok(n) => Err(format!("port {n} out of range 1..65535")),
        Err(_) => Err(format!("port {s} out of range 1..65535")),
    }
}

/// Turns an IPv6 literal or a hostname into one IPv6 address. IPv4 (including
/// IPv4-mapped IPv6) is refused: the target must be reachable over IPv6.
pub fn resolve_dest(s: &str) -> Result<Ipv6Addr, String> {
    if let Ok(ip) = s.parse::<IpAddr>() {
        return match ip {
            IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => Ok(v6),
            _ => Err(format!(
                "fwknop.dest \"{s}\" is IPv4; an IPv6 SPA target is required"
            )),
        };
    }
    let addrs = (s, 0)
        .to_socket_addrs()
        .map_err(|e| format!("resolving fwknop.dest \"{s}\": {e}"))?;
    addrs
        .filter_map(|a| match a.ip() {
            IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => Some(v6),
            _ => None,
        })
        .next()
        .ok_or_else(|| format!("fwknop.dest \"{s}\" has no AAAA (IPv6) address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FWKNOP: &str = r#"
[fwknop]
dest     = "2001:db8::1"
port     = 62159
key_ref  = "op://vault/item/key"
hmac_ref = "op://vault/item/hmac"
"#;

    fn load_str(text: &str) -> Result<Config, String> {
        parse(text, Path::new("test.toml"), resolve_dest)
    }

    fn with_tunables(tunables: &str) -> Result<Config, String> {
        load_str(&format!("{tunables}\n{FWKNOP}"))
    }

    fn err(text: &str) -> String {
        load_str(text).expect_err("expected an error")
    }

    #[test]
    fn defaults() {
        let cfg = with_tunables("").unwrap();
        assert_eq!(cfg.debounce, Duration::from_secs(2));
        assert_eq!(cfg.reconcile, Duration::from_secs(300));
        assert_eq!(cfg.refresh, None);
        assert_eq!(cfg.timeout, Duration::from_secs(60));
        assert_eq!(cfg.attempts, 3);
        assert_eq!(cfg.op_path, None);
        assert_eq!(
            cfg.fwknop.target,
            "2001:db8::1".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(cfg.fwknop.port, 62159);
        assert_eq!(cfg.fwknop.access, vec!["tcp/443".parse().unwrap()]);
    }

    #[test]
    fn refresh() {
        let cases: &[(&str, Result<Option<Duration>, &str>)] = &[
            (r#"refresh = "10m""#, Ok(Some(Duration::from_secs(600)))),
            (r#"refresh = "0s""#, Ok(None)),
            (r#"refresh = "0""#, Ok(None)),
            (r#"refresh = """#, Ok(None)),
            (r#"refresh = "-1s""#, Err("refresh must be positive")),
            (
                r#"refresh = "soon""#,
                Err("refresh: time: invalid duration"),
            ),
        ];
        for (tunables, want) in cases {
            match (with_tunables(tunables), want) {
                (Ok(cfg), Ok(w)) => assert_eq!(cfg.refresh, *w, "{tunables}"),
                (Err(e), Err(w)) => assert!(e.contains(w), "{tunables}: {e}"),
                (got, _) => panic!("{tunables}: unexpected {got:?}"),
            }
        }
    }

    #[test]
    fn go_duration_syntax_is_accepted() {
        let cfg = with_tunables(
            "debounce = \"300ms\"\nreconcile = \"1h30m\"\nrefresh = \"1.5h\"\ntimeout = \"90s\"",
        )
        .unwrap();
        assert_eq!(cfg.debounce, Duration::from_millis(300));
        assert_eq!(cfg.reconcile, Duration::from_secs(5400));
        assert_eq!(cfg.refresh, Some(Duration::from_secs(5400)));
        assert_eq!(cfg.timeout, Duration::from_secs(90));
    }

    #[test]
    fn zero_rules() {
        assert_eq!(
            with_tunables("debounce = \"0s\"").unwrap().debounce,
            Duration::ZERO
        );
        assert!(
            with_tunables("reconcile = \"0s\"")
                .unwrap_err()
                .contains("reconcile must be positive")
        );
        assert!(
            with_tunables("timeout = \"0\"")
                .unwrap_err()
                .contains("timeout must be positive")
        );
        assert!(
            with_tunables("retries = 0")
                .unwrap_err()
                .contains("retries must be >= 1")
        );
        assert_eq!(with_tunables("retries = 5").unwrap().attempts, 5);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(
            with_tunables("debounse = \"2s\"")
                .unwrap_err()
                .contains("debounse")
        );
        let e = err(&FWKNOP.replace("port ", "prot "));
        assert!(e.contains("prot"), "{e}");
    }

    #[test]
    fn op_path() {
        let cfg = with_tunables("op_path = \"/usr/local/bin/op\"").unwrap();
        assert_eq!(cfg.op_path, Some(PathBuf::from("/usr/local/bin/op")));
        assert_eq!(with_tunables("op_path = \"\"").unwrap().op_path, None);
    }

    #[test]
    fn required_fwknop_settings() {
        assert!(err("debounce = \"2s\"").contains("missing [fwknop] section"));
        assert!(
            err(&FWKNOP.replace("dest     = \"2001:db8::1\"", ""))
                .contains("fwknop.dest is required")
        );
        assert!(err(&FWKNOP.replace("62159", "0")).contains("fwknop.port must be in 1..65535"));
        assert!(err(&FWKNOP.replace("62159", "65536")).contains("fwknop.port must be in 1..65535"));
        assert!(
            err(&FWKNOP.replace("key_ref  = \"op://vault/item/key\"", ""))
                .contains("fwknop.key_ref is required")
        );
        assert!(
            err(&FWKNOP.replace("hmac_ref = \"op://vault/item/hmac\"", ""))
                .contains("fwknop.hmac_ref is required")
        );
    }

    #[test]
    fn access_entries() {
        let with_access = |a: &str| load_str(&format!("{FWKNOP}access = {a}\n"));
        let cfg = with_access(r#"["tcp/443", "udp/53", "tcp/22"]"#).unwrap();
        let shown: Vec<String> = cfg.fwknop.access.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["tcp/443", "udp/53", "tcp/22"]);

        let bad = |a: &str, want: &str| {
            let e = with_access(a).expect_err(a);
            assert!(e.contains(want), "{a}: {e}");
        };
        bad("[]", "fwknop.access is empty");
        bad(r#"["tcp/443,tcp/22"]"#, "one protocol/port per entry");
        bad(r#"["tcp/443 tcp/22"]"#, "one protocol/port per entry");
        bad(r#"["443"]"#, "expected \"proto/port\"");
        bad(r#"["icmp/0"]"#, "protocol \"icmp\" must be tcp or udp");
        bad(r#"["TCP/22"]"#, "must be tcp or udp");
        bad(r#"["tcp/1000-2000"]"#, "port ranges are not supported");
        bad(r#"["tcp/ssh"]"#, "port \"ssh\" is not a number");
        bad(r#"["tcp/+22"]"#, "is not a number");
        bad(r#"["tcp/0"]"#, "port 0 out of range");
        bad(r#"["udp/70000"]"#, "port 70000 out of range");
    }

    #[test]
    fn dest_must_be_ipv6() {
        assert!(resolve_dest("192.0.2.1").unwrap_err().contains("is IPv4"));
        assert!(
            resolve_dest("::ffff:192.0.2.1")
                .unwrap_err()
                .contains("is IPv4")
        );
        assert_eq!(resolve_dest("::1").unwrap(), Ipv6Addr::LOCALHOST);
        assert!(resolve_dest("2001:db8::1").is_ok());
    }

    #[test]
    fn dest_is_resolved_once_through_the_resolver() {
        let calls = std::cell::Cell::new(0);
        let cfg = parse(
            &FWKNOP.replace("2001:db8::1", "spa.example.com"),
            Path::new("t"),
            |host| {
                calls.set(calls.get() + 1);
                assert_eq!(host, "spa.example.com");
                Ok("2001:db8::7".parse().unwrap())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(cfg.fwknop.dest, "spa.example.com");
        assert_eq!(
            cfg.fwknop.target,
            "2001:db8::7".parse::<Ipv6Addr>().unwrap()
        );
    }
}
