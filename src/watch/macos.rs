//! macOS: there is no rtnetlink, so poll the interface address table every
//! 5 seconds and signal when it changes (ADR-0004).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;

use super::notify;
use crate::log_warn;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub fn default_config_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/ipv6rd/ipv6rd.toml"),
        None => PathBuf::from("~/.config/ipv6rd/ipv6rd.toml"),
    }
}

/// Starts the polling thread; it exits when the receiving side of `out` is gone.
pub fn spawn(out: Sender<()>) {
    thread::Builder::new()
        .name("addr-watch".into())
        .spawn(move || {
            let mut snapshot = current_addrs();
            loop {
                thread::sleep(POLL_INTERVAL);
                let now = current_addrs();
                if now != snapshot {
                    snapshot = now;
                    if !notify(&out) {
                        return;
                    }
                }
            }
        })
        .expect("starting the address watcher");
}

/// Every interface address with its netmask, e.g. `en0 2001:db8::1/ffff:…`.
fn current_addrs() -> BTreeSet<String> {
    match nix::ifaddrs::getifaddrs() {
        Ok(addrs) => addrs
            .filter_map(|ifa| {
                let addr = ifa.address?;
                let mask = ifa.netmask.map(|m| m.to_string()).unwrap_or_default();
                Some(format!("{} {addr}/{mask}", ifa.interface_name))
            })
            .collect(),
        Err(e) => {
            log_warn!("interface list failed", "err" => e);
            BTreeSet::new()
        }
    }
}
