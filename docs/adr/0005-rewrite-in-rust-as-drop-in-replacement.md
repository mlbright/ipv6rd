# Rewrite in Rust as a drop-in replacement

ipv6rd is rewritten in Rust, in place in this repository, and the Go code is
removed. The rewrite is a drop-in replacement: same binary name, same `--config`
flag, same TOML keys and defaults (Go-style duration strings such as `"1h30m"` and
`"1.5h"` keep parsing), same file paths, same `OP_SERVICE_ACCOUNT_TOKEN` variable,
same systemd unit, launchd agent and wrapper, and the same `make` targets (now
driving `cargo`). An existing deployment upgrades by swapping the binary (plus the
`op` CLI requirement from [ADR-0007](./0007-keys-via-op-cli.md)).

It is a faithful port of the watcher's behaviour. That covers rtnetlink events on
Linux and 5 s polling on macOS, the debounce, reconcile and refresh timers, at most
one knock in flight with converge-to-latest, retries with 2 s → 8 s → 32 s backoff
and a per-attempt timeout, one SPA packet per access entry, and the
`level=… msg=… key=value` log lines on stderr. The only intended behaviour changes
are the security hardening recorded in [ADR-0006](./0006-spa-security-posture.md).

## Dependencies

The dependency tree is kept small on purpose, because every crate is code that
runs next to the fwknop keys. There is no async runtime: the event loop is plain
threads with `crossbeam-channel`'s `select!`, the closest match to the Go loop.
Linux address events come from a raw netlink socket that only needs to know
"an address changed", not from the `rtnetlink` stack. `Cargo.lock` is committed.
`make audit` runs `cargo deny`, which:

- fails on any RustSec security advisory;
- allows only licenses compatible with GPL-2.0-only ([ADR-0008](./0008-spa-encoding-via-fkospa.md)),
  and so rejects Apache-2.0-only crates;
- allows sources from crates.io and the pinned fwknop-rs repository only.

## Considered and rejected

- **Build the Rust version alongside the Go one** (new name or directory) and cut
  over later. Rejected: two implementations to keep in step for no benefit; the
  drop-in constraint already protects existing deployments.
