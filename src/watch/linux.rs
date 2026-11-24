//! Linux: a raw rtnetlink socket subscribed to IPv6 address events. The
//! messages are not parsed — any event (or a lost-events overrun) just means
//! "recompute the egress address", which the main loop debounces.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;
use nix::errno::Errno;
use nix::sys::socket::{
    AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, recv, socket,
};

use super::notify;
use crate::log_warn;

/// The rtnetlink multicast group for IPv6 address changes.
const RTMGRP_IPV6_IFADDR: u32 = 0x100;

pub fn default_config_path() -> PathBuf {
    PathBuf::from("/etc/ipv6rd/ipv6rd.toml")
}

/// Starts the watcher thread. It resubscribes if the socket fails, and exits
/// when the receiving side of `out` is gone.
pub fn spawn(out: Sender<()>) {
    thread::Builder::new()
        .name("addr-watch".into())
        .spawn(move || watch(&out))
        .expect("starting the address watcher");
}

fn watch(out: &Sender<()>) {
    loop {
        let fd = match subscribe() {
            Ok(fd) => fd,
            Err(e) => {
                log_warn!("address-event subscribe failed; retrying", "err" => e);
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        match drain(&fd, out) {
            None => return, // receiver gone
            Some(e) => {
                log_warn!("netlink subscription error; resubscribing", "err" => e);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn subscribe() -> nix::Result<OwnedFd> {
    let fd = socket(
        AddressFamily::Netlink,
        SockType::Raw,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::NetlinkRoute,
    )?;
    bind(fd.as_raw_fd(), &NetlinkAddr::new(0, RTMGRP_IPV6_IFADDR))?;
    Ok(fd)
}

/// Signals on every message until the socket fails (returns the error) or the
/// receiver is gone (returns `None`).
fn drain(fd: &OwnedFd, out: &Sender<()>) -> Option<Errno> {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match recv(fd.as_raw_fd(), &mut buf, MsgFlags::empty()) {
            // ENOBUFS: the kernel dropped events because we fell behind;
            // recompute anyway.
            Ok(_) | Err(Errno::ENOBUFS) => {
                if !notify(out) {
                    return None;
                }
            }
            Err(Errno::EINTR) => {}
            Err(e) => return Some(e),
        }
    }
}
