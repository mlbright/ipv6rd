//! ipv6rd watches the host's outbound (egress) IPv6 address and re-knocks an
//! fwknop SPA grant whenever that address rotates, keeping the grant pinned to
//! a rotating IPv6 temporary/dynamic address.
//!
//! All settings come from a TOML file; the only input from outside it is the
//! 1Password service-account token in `OP_SERVICE_ACCOUNT_TOKEN`. The fwknop
//! keys are fetched once at startup with `op read` (ADR-0007) and every SPA
//! packet is built with fwknop-rs's `fkospa` crate (ADR-0008). The security
//! posture — one fixed scheme, one pinned target address, packets sent from the
//! address they authorize — is ADR-0006.

pub mod config;
pub mod controller;
pub mod daemon;
pub mod duration;
pub mod egress;
pub mod harden;
pub mod keys;
pub mod knock;
pub mod log;
pub mod onepassword;
pub mod watch;
