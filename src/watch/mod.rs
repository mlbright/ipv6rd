//! Platform-specific address-change detection (ADR-0004). Each platform
//! provides `spawn(out)`, which sends a coalesced signal on `out` whenever the
//! host's addresses may have changed, and `default_config_path()`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{default_config_path, spawn};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{default_config_path, spawn};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ipv6rd supports Linux and macOS only");

use crossbeam_channel::{Sender, TrySendError};

/// Sends one signal unless one is already pending. Returns false once the
/// receiver is gone.
fn notify(out: &Sender<()>) -> bool {
    !matches!(out.try_send(()), Err(TrySendError::Disconnected(())))
}
