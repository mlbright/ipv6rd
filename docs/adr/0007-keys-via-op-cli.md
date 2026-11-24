# Fetch the fwknop keys with the `op` CLI

**Supersedes in part [ADR-0002](./0002-in-process-fwknop-knock-via-library-and-1password-sdk.md):**
the keys still come from 1Password through a service account and are still resolved
once at startup, but by running `op read op://…` instead of calling the 1Password
Go SDK.

The Rust rewrite ([ADR-0005](./0005-rewrite-in-rust-as-drop-in-replacement.md))
has no official 1Password SDK to call. 1Password ships SDKs only for Go, Python and
JavaScript, all wrappers around a Rust core whose interface is undocumented and not
supported for outside use. Running `op` with `OP_SERVICE_ACCOUNT_TOKEN` is
1Password's documented headless path for service accounts (CLI 2.18+). It needs no
desktop app or sign-in session, so ADR-0002's objection about an interactive
session no longer applies. The remaining cost is that hosts need `op` installed.

## How `op` is run

- **Finding the binary.** The optional top-level key `op_path` names the binary.
  Without it, `op` is looked up on `PATH` once at startup and the absolute path is
  logged. ipv6rd refuses a binary that is group- or world-writable, or owned by
  anyone other than root or the daemon's user. The macOS wrapper adds
  `/opt/homebrew/bin:/usr/local/bin` to `PATH`, since launchd's default has neither.
- **Environment.** The child starts with an empty environment plus only
  `OP_SERVICE_ACCOUNT_TOKEN`, `HOME`, `OP_CACHE=false` (no background process
  keeps the token) and, on macOS, `OP_LOAD_DESKTOP_APP_SETTINGS=false` (the
  reported workaround for a repeated permission prompt under launchd). Under
  systemd, `OP_CONFIG_DIR` points into the unit's `RuntimeDirectory`. A stray
  `OP_CONNECT_*` variable therefore can never override the service account.
- **Calls.** `op read --no-newline <ref>` once per key. Stdout is read from a pipe
  into a buffer that is zeroed after decoding. Stderr is logged, capped in size.
  Each call times out after 30 s and the child is then killed. The `op://`
  references go on the command line because they are not secret: they are already
  in the world-readable config.

## Restart loops and the 1Password rate limit

Each `op read` counts as 3 requests against the service account's hourly limit
(1,000, or 10,000 on Business), and that limit is shared with every other user of
the account. A startup that fails *after* fetching the keys would, under a plain
5-second restart, use about 4,300 requests an hour. That happens, for example, when
the key rules in ADR-0006 reject a key. So:

- Config errors and key-rule failures exit with code 2, and the systemd unit sets
  `RestartPreventExitStatus=2`. A misconfiguration stops instead of looping.
- Other startup failures (for example `op` cannot reach 1Password) restart with a
  delay growing from 5 s to 15 min (`RestartSteps=10`, `RestartMaxDelaySec=15min`,
  systemd 254+).
- launchd cannot tell exit codes apart, so the agent sets `ThrottleInterval` to
  300 s, about 72 requests an hour at worst.

## Considered and rejected

- **Run 1Password's SDK core from Rust** (the `core.wasm` inside the Go SDK, via
  Extism). Rejected: undocumented, unsupported interface that can break without
  notice on any core update.
- **A small Go helper using the official SDK.** Rejected: a second binary plus a Go
  toolchain at build time, to avoid one external binary.
- **Drop 1Password; read the keys from a `0600` file or OS credential store.**
  Rejected for now: it reverses ADR-0002 and changes how keys are rotated.
- **1Password Connect server.** Rejected: self-hosted infrastructure and a
  different kind of token, which breaks the drop-in constraint.
- **`corteq-onepassword` crate.** Rejected: AGPL-3.0, downloads a native library
  from PyPI at build time, and pinned to an outdated SDK.
