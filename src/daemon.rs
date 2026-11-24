//! Startup and the event loop.

use std::net::{Ipv6Addr, SocketAddrV6};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, at, bounded, never, select, tick};
use zeroize::Zeroizing;

use crate::config::Config;
use crate::controller::Controller;
use crate::duration::GoDuration;
use crate::egress::egress_addr;
use crate::knock::Knocker;
use crate::log::List;
use crate::onepassword::{self, OpClient, ReadError};
use crate::{keys, log_info, log_warn, watch};

/// Why the daemon stopped with an error.
#[derive(Debug)]
pub enum Fatal {
    /// Misconfiguration that a restart cannot fix (exit code 2; the systemd
    /// unit does not restart on it, so no 1Password requests are wasted).
    Config(String),
    /// Anything else (exit code 1; restarted with backoff).
    Runtime(String),
}

/// Starts the daemon and runs until `shutdown` disconnects.
pub fn run(
    cfg: Config,
    token: Option<Zeroizing<String>>,
    shutdown: Receiver<()>,
) -> Result<(), Fatal> {
    let refresh_desc = cfg
        .refresh
        .map_or("(disabled)".to_string(), |r| GoDuration(r).to_string());
    let access: Vec<String> = cfg.fwknop.access.iter().map(ToString::to_string).collect();
    log_info!("starting",
        "spa_target" => cfg.fwknop.dest, "dest_ip" => cfg.fwknop.target,
        "spa_port" => cfg.fwknop.port, "access" => List(&access),
        "debounce" => GoDuration(cfg.debounce), "reconcile" => GoDuration(cfg.reconcile),
        "refresh" => refresh_desc,
        "timeout" => GoDuration(cfg.timeout), "attempts" => cfg.attempts);

    // Fetch the keys up front (fail fast) and keep them for the process's life.
    let Some(keys) = fetch_keys(&cfg, token, &shutdown)? else {
        return Ok(()); // shut down while fetching
    };

    let knocker = Arc::new(Knocker {
        target: SocketAddrV6::new(cfg.fwknop.target, cfg.fwknop.port, 0, 0),
        access: cfg.fwknop.access.clone(),
        keys,
    });

    let (events_tx, events_rx) = bounded(1);
    watch::spawn(events_tx);
    event_loop(&cfg, knocker, events_rx, shutdown);
    Ok(())
}

/// Resolves both keys with `op read` and checks them against the key rules.
/// Returns `None` if shutdown was requested meanwhile.
fn fetch_keys(
    cfg: &Config,
    token: Option<Zeroizing<String>>,
    shutdown: &Receiver<()>,
) -> Result<Option<fkospa::KeySet>, Fatal> {
    let token = token.ok_or_else(|| Fatal::Config("OP_SERVICE_ACCOUNT_TOKEN is not set".into()))?;
    let found = onepassword::locate(cfg.op_path.as_deref(), std::env::var_os("PATH").as_deref())
        .map_err(Fatal::Config)?;
    let op = onepassword::check_binary(&found, nix::unistd::geteuid().as_raw())
        .map_err(Fatal::Config)?;
    log_info!("using the 1Password CLI", "path" => op.display());

    // The client owns the token and drops (zeroes) it when this function ends.
    let client = OpClient::new(
        op,
        token,
        std::env::var_os("HOME"),
        std::env::var_os("OP_CONFIG_DIR"),
        onepassword::READ_TIMEOUT,
    );
    let read = |what: &str, reference: &str| match client.read_key(reference, shutdown) {
        Ok(k) => Ok(Some(k)),
        Err(ReadError::Cancelled) => Ok(None),
        Err(ReadError::Failed(e)) => Err(Fatal::Runtime(format!(
            "resolving {what} \"{reference}\": {e}"
        ))),
    };
    let Some(enc) = read("encryption key", &cfg.fwknop.key_ref)? else {
        return Ok(None);
    };
    let Some(hmac) = read("HMAC key", &cfg.fwknop.hmac_ref)? else {
        return Ok(None);
    };
    drop(client);

    let keys = keys::build_keyset(enc, hmac).map_err(Fatal::Config)?;
    log_info!("resolved fwknop keys from 1Password",
        "key_ref" => cfg.fwknop.key_ref, "hmac_ref" => cfg.fwknop.hmac_ref);
    Ok(Some(keys))
}

type FireResult = (Ipv6Addr, Result<(), String>);

fn event_loop(cfg: &Config, knocker: Arc<Knocker>, events: Receiver<()>, shutdown: Receiver<()>) {
    let (done_tx, done_rx) = bounded::<FireResult>(1);
    let start = |target: Ipv6Addr| {
        let (knocker, shutdown, done_tx) =
            (Arc::clone(&knocker), shutdown.clone(), done_tx.clone());
        let (attempts, timeout) = (cfg.attempts, cfg.timeout);
        thread::spawn(move || {
            let res = fire_with_retry(attempts, timeout, target, &shutdown, |a, t| {
                knocker.knock(a, t)
            });
            let _ = done_tx.send((target, res));
        });
    };
    let recompute = |ctrl: &mut Controller, reason: &str| match egress_addr(cfg.fwknop.target) {
        Err(e) => log_warn!("egress lookup failed", "reason" => reason, "err" => e),
        Ok(None) => {
            log_warn!("no egress source address (no IPv6 connectivity?)", "reason" => reason)
        }
        Ok(Some(addr)) => {
            if let Some(target) = ctrl.observe(reason, addr) {
                start(target);
            }
        }
    };

    let mut ctrl = Controller::new(cfg.refresh, Instant::now());
    let reconcile = tick(cfg.reconcile);
    let mut debounce: Option<Instant> = None;
    let mut events = events;

    // Fire once on startup with the current address.
    recompute(&mut ctrl, "startup");

    loop {
        let debounce_rx = debounce.map_or_else(never, at);
        let refresh_rx = ctrl.refresh_due().map_or_else(never, at);
        select! {
            recv(shutdown) -> _ => {
                log_info!("shutdown signal received, stopping");
                return;
            }
            recv(events) -> msg => match msg {
                // Coalesce a burst of address events: (re)arm the debounce and
                // recompute only once it goes quiet.
                Ok(()) => debounce = Some(Instant::now() + cfg.debounce),
                Err(_) => {
                    log_warn!("address watcher stopped; relying on the reconcile poll");
                    events = never();
                }
            },
            recv(debounce_rx) -> _ => {
                debounce = None;
                recompute(&mut ctrl, "address-event");
            }
            recv(reconcile) -> _ => recompute(&mut ctrl, "reconcile"),
            recv(refresh_rx) -> _ => {
                ctrl.refresh_elapsed(Instant::now());
                recompute(&mut ctrl, "refresh");
            }
            recv(done_rx) -> msg => {
                let (target, res) = msg.expect("the loop holds a sender");
                if let Some(next) = ctrl.fire_done(target, res, Instant::now()) {
                    start(next);
                }
            }
        }
    }
}

/// Runs `knock` for `addr`, retrying with bounded exponential backoff up to
/// `attempts` times and giving each attempt `timeout`.
pub fn fire_with_retry(
    attempts: u32,
    timeout: Duration,
    addr: Ipv6Addr,
    shutdown: &Receiver<()>,
    knock: impl Fn(Ipv6Addr, Duration) -> Result<(), String>,
) -> Result<(), String> {
    let mut last_err = String::new();
    for attempt in 1..=attempts {
        if attempt > 1 {
            let backoff = backoff_delay(attempt - 1);
            log_warn!("retrying", "addr" => addr, "attempt" => attempt, "of" => attempts,
                "after" => GoDuration(backoff));
            select! {
                recv(shutdown) -> _ => return Err("shutting down".into()),
                default(backoff) => {}
            }
        }
        match knock(addr, timeout) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log_warn!("attempt failed", "addr" => addr, "attempt" => attempt, "err" => e);
                last_err = e;
                if matches!(shutdown.try_recv(), Err(TryRecvError::Disconnected)) {
                    return Err("shutting down".into());
                }
            }
        }
    }
    Err(format!("all {attempts} attempts failed: {last_err}"))
}

/// The wait before retry `n` (1-indexed): 2s, 8s, 32s, … capped at 1 hour.
pub fn backoff_delay(n: u32) -> Duration {
    let factor = 4u64.saturating_pow(n.saturating_sub(1));
    Duration::from_secs(2u64.saturating_mul(factor).min(3600))
}

/// Blocks SIGINT and SIGTERM in this and every later thread, and returns a
/// channel that disconnects when either arrives. Call before starting threads.
pub fn shutdown_on_signals() -> Result<Receiver<()>, String> {
    use nix::sys::signal::{SigSet, Signal};
    let mut set = SigSet::empty();
    set.add(Signal::SIGINT);
    set.add(Signal::SIGTERM);
    set.thread_block()
        .map_err(|e| format!("blocking signals: {e}"))?;
    let (tx, rx): (Sender<()>, Receiver<()>) = bounded(0);
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let _ = set.wait();
            drop(tx);
        })
        .map_err(|e| format!("starting the signal thread: {e}"))?;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn backoff() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(8));
        assert_eq!(backoff_delay(3), Duration::from_secs(32));
        assert_eq!(backoff_delay(40), Duration::from_secs(3600));
    }

    #[test]
    fn retries_until_success() {
        let (_tx, rx) = bounded::<()>(0);
        let calls = Cell::new(0);
        // attempts=1 means no backoff sleep is reached.
        let res = fire_with_retry(
            1,
            Duration::from_secs(1),
            Ipv6Addr::LOCALHOST,
            &rx,
            |_, _| {
                calls.set(calls.get() + 1);
                Err("nope".into())
            },
        );
        assert_eq!(res.unwrap_err(), "all 1 attempts failed: nope");
        assert_eq!(calls.get(), 1);

        let ok = fire_with_retry(
            3,
            Duration::from_secs(1),
            Ipv6Addr::LOCALHOST,
            &rx,
            |_, _| Ok(()),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn shutdown_interrupts_the_backoff() {
        let (tx, rx) = bounded::<()>(0);
        drop(tx);
        let start = Instant::now();
        let res = fire_with_retry(
            3,
            Duration::from_secs(1),
            Ipv6Addr::LOCALHOST,
            &rx,
            |_, _| Err("x".into()),
        );
        assert_eq!(res.unwrap_err(), "shutting down");
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
