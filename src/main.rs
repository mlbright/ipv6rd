//! `ipv6rd [--config PATH]` — see the crate docs and README.
//!
//! Exit codes: 0 after a shutdown signal; 2 for usage and configuration errors
//! (including a missing token, a missing or unsafe `op`, and keys that break
//! the key rules), which the systemd unit does not restart; 1 for anything
//! else, which it restarts with backoff (ADR-0007).

use std::path::PathBuf;
use std::process::ExitCode;

use ipv6rd::daemon::{self, Fatal};
use ipv6rd::{config, harden, log_error, watch};

fn main() -> ExitCode {
    // Order matters: no core dumps before any secret is in memory, and the
    // token leaves the environment before any thread exists.
    if let Err(e) = harden::disable_core_dumps() {
        log_error!("exiting", "err" => e);
        return ExitCode::from(1);
    }
    let token = harden::take_env_secret("OP_SERVICE_ACCOUNT_TOKEN");
    let shutdown = match daemon::shutdown_on_signals() {
        Ok(rx) => rx,
        Err(e) => {
            log_error!("exiting", "err" => e);
            return ExitCode::from(1);
        }
    };

    let default_path = watch::default_config_path();
    let config_path = match parse_args(std::env::args().skip(1), &default_path) {
        Ok(p) => p,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("{msg}");
            }
            eprint!("{}", usage(&default_path));
            return ExitCode::from(2);
        }
    };

    let cfg = match config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("ipv6rd: {e}");
            return ExitCode::from(2);
        }
    };

    match daemon::run(cfg, token, shutdown) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fatal::Config(e)) => {
            log_error!("exiting", "err" => e);
            ExitCode::from(2)
        }
        Err(Fatal::Runtime(e)) => {
            log_error!("exiting", "err" => e);
            ExitCode::from(1)
        }
    }
}

/// Parses flags the way Go's `flag` package did for the Go version: `-config`
/// or `--config`, with the value as `=VALUE` or the next argument; parsing
/// stops at `--` or the first non-flag argument. `Err("")` asks for help.
fn parse_args(
    args: impl IntoIterator<Item = String>,
    default: &std::path::Path,
) -> Result<PathBuf, String> {
    let mut path = default.to_path_buf();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--" || !arg.starts_with('-') || arg == "-" {
            break;
        }
        let flag = arg.trim_start_matches('-');
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (flag, None),
        };
        match name {
            "h" | "help" => return Err(String::new()),
            "config" => {
                let value = match inline {
                    Some(v) => v,
                    None => args.next().ok_or("flag needs an argument: -config")?,
                };
                path = PathBuf::from(value);
            }
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
    }
    Ok(path)
}

fn usage(default: &std::path::Path) -> String {
    format!(
        "usage: ipv6rd [--config PATH]\n\n\
         Re-knocks an fwknop SPA grant whenever the egress IPv6 address rotates.\n\
         All settings are read from the TOML config file (default {d}); the\n\
         1Password service-account token is read from OP_SERVICE_ACCOUNT_TOKEN.\n\n\
         flags:\n  -config string\n    \tpath to the TOML configuration file (default \"{d}\")\n",
        d = default.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(args: &[&str]) -> Result<PathBuf, String> {
        parse_args(
            args.iter().map(|s| s.to_string()),
            Path::new("/etc/ipv6rd/ipv6rd.toml"),
        )
    }

    #[test]
    fn flag_forms() {
        let want = PathBuf::from("/tmp/x.toml");
        assert_eq!(
            parse(&[]).unwrap(),
            PathBuf::from("/etc/ipv6rd/ipv6rd.toml")
        );
        for args in [
            &["--config", "/tmp/x.toml"][..],
            &["-config", "/tmp/x.toml"],
            &["--config=/tmp/x.toml"],
            &["-config=/tmp/x.toml"],
            &["--config", "/tmp/x.toml", "ignored", "--bogus"],
        ] {
            assert_eq!(parse(args).unwrap(), want, "{args:?}");
        }
    }

    #[test]
    fn errors_and_help() {
        assert_eq!(parse(&["--help"]).unwrap_err(), "");
        assert_eq!(parse(&["-h"]).unwrap_err(), "");
        assert_eq!(
            parse(&["--config"]).unwrap_err(),
            "flag needs an argument: -config"
        );
        assert_eq!(
            parse(&["--fwknop-dest", "x"]).unwrap_err(),
            "flag provided but not defined: -fwknop-dest"
        );
    }
}
