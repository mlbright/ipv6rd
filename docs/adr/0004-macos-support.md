# macOS support: poll-based watcher, launchd user agent

ipv6rd now supports macOS alongside Linux. The two platforms share all business
logic (debounce, re-knock, key resolution, config parsing) and diverge only in how
address changes are detected and how the service is managed.

**Address watching.** On macOS there is no equivalent of Linux's `rtnetlink`
`AddrSubscribe`. The watcher polls `net.Interfaces()` (which calls `getifaddrs()`
under the hood) every 5 seconds, comparing the current address set against the
previous snapshot. When the sets differ it emits the same coalesced signal the Linux
path emits on a netlink event. The 5-second interval is hardcoded — it is not
exposed in the TOML config, which would otherwise require a platform-specific key
that does nothing on Linux.

**Egress address selection.** Both platforms now use the `net.Dial` trick instead of
`netlink.RouteGet` (see ADR-0001 update). This change was driven by macOS support
but applies to Linux too: the semantics are identical and the code is simpler.

**Service management.** The service runs as a launchd *user agent*
(`~/Library/LaunchAgents/com.ipv6rd.ipv6rd.plist`) rather than a system daemon
(`/Library/LaunchDaemons/`). It starts at login, runs as the current user, and
requires no root to install.

**Token delivery.** Launchd has no `EnvironmentFile` equivalent. Rather than embed
`OP_SERVICE_ACCOUNT_TOKEN` in the plist (which is world-readable by convention), a
wrapper script at `$PREFIX/bin/ipv6rd-wrapper` sources a `0600` env file at
`~/.config/ipv6rd/ipv6rd.env` before exec-ing the binary. The launchd plist launches
the wrapper, not the binary directly. This mirrors the Linux pattern (systemd
`EnvironmentFile=`) without requiring new code in the binary.

**Config location.** On macOS the default config path is
`~/.config/ipv6rd/ipv6rd.toml` (expanded at startup via `os.UserHomeDir()`). On
Linux it remains `/etc/ipv6rd/ipv6rd.toml`. The `--config` override works on both.

**Code structure.** Platform-specific code lives in build-tag-split files:
`watcher_linux.go` (rtnetlink subscription) and `watcher_darwin.go` (polling). Both
implement `watchAddrEvents(ctx, out)` and `platformDefaultConfigPath()`. All other
code is shared in `main.go`.

## Consequences

- `make install-launchd` installs the binary, wrapper script, launchd plist, and
  scaffolds `~/.config/ipv6rd/ipv6rd.toml` and `~/.config/ipv6rd/ipv6rd.env`. No
  root required.
- The 5-second poll interval means address changes are detected up to 5s later than
  on Linux. This is acceptable: the re-knock completes well within any timeout window
  on the `fwknopd` side.
- The binary only compiles for `linux` and `darwin`; other platforms get a build
  error at the watcher file.

## Considered and rejected

- **BSD route socket (`AF_ROUTE`)** for event-driven address notifications on macOS.
  Rejected: significantly more complex to implement correctly; the `RTM_NEWADDR` /
  `RTM_DELADDR` interface is underdocumented on macOS. The 5s poll lag is not
  meaningful for this use case.
- **macOS launchd system daemon** (`/Library/LaunchDaemons/`). Rejected: requires
  root to install, runs before login, and is inappropriate for a single-user tool
  that needs access to a user's 1Password service-account token.
- **`EnvironmentVariables` key in the plist** for token delivery. Rejected: the plist
  lives in `~/Library/LaunchAgents/` and is conventionally world-readable; the
  wrapper + 0600 env file keeps the secret off a readable file.
- **XDG `$XDG_CONFIG_HOME`** for the macOS config path. Rejected: `os.UserHomeDir()`
  is sufficient; `XDG_CONFIG_HOME` is rarely set on macOS and adds complexity for no
  practical gain.
