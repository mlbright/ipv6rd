# ipv6rd

Watches the host's **egress IPv6 address** and re-knocks an
[fwknop](https://github.com/mrash/fwknop) Single Packet Authorization grant
whenever that address rotates, so you don't get locked out when your source IP
changes.

It builds and sends the SPA packets itself, in process, with the `fkospa` crate
from [fwknop-rs](https://github.com/mlbright/fwknop-rs) — the same code the
fwknop-rs server decodes with. The encryption and HMAC keys are fetched once at
startup from [1Password](https://developer.1password.com/docs/service-accounts/)
with the `op` CLI and a service account.

Everything is configured by a single TOML file. The only input from outside the
file is the 1Password service-account token, read from the environment.

See [`CONTEXT.md`](./CONTEXT.md) for terminology and
[`docs/adr/`](./docs/adr/) for design decisions.

## Requirements

- **Linux** or **macOS**, with working IPv6.
- An **SPA target** running fwknop-rs's `fwknopd`, reachable over IPv6. Stock C
  fwknopd cannot be used: it only accepts IPv4 allow IPs and listens on IPv4 only.
- The **1Password CLI** (`op`, 2.18 or later) and a **service-account token** that
  can read the two keys.
- To build: Rust 1.99.1 or later.

## How it works

1. Watches for address changes: rtnetlink IPv6 address events on Linux, a 5 s poll
   of the interface table on macOS.
2. On each change (debounced, ~2 s), on a periodic reconciliation poll, and at
   startup, it asks the kernel which source address it would use to reach the
   **SPA target**, by connecting a UDP socket without sending anything. That source
   is the egress address. See
   [ADR 0001](./docs/adr/0001-egress-address-via-route-get.md).
3. If the egress address differs from the last one it knocked, it re-knocks: one
   SPA packet per [access request](./CONTEXT.md) entry, each authorizing the egress
   address, each sent from that address.

It fires once on startup, runs one knock at a time (converging to the latest
address if it rotates again mid-knock), and retries a failed knock with bounded
exponential backoff (2s → 8s → 32s) before waiting for the next event. With
`refresh` set, it also re-knocks the *unchanged* address whenever that long has
passed since the last successful knock — a grant keep-alive for hosts whose egress
address never rotates. State is in memory only.

## Security

What ipv6rd does to keep its exchange with the SPA target safe
([ADR 0006](./docs/adr/0006-spa-security-posture.md)):

- **One scheme.** Every packet is Rijndael (AES-256) in CBC mode with an
  HMAC-SHA256 over the ciphertext. There is no setting for GPG, the legacy IV mode,
  other cipher modes, other digests, or HMAC-less packets.
- **One target address.** `fwknop.dest` is looked up once at startup. That single
  IPv6 address is both the probe destination and where every packet goes, with no
  IPv4 fallback and no further DNS lookups.
- **Source equals allow IP.** Each packet is sent from a socket bound to the egress
  address it authorizes, so the server's `require_source_address` check always
  holds. If the address has disappeared, the send fails instead of sending a packet
  the server would drop.
- **Random keys only.** The encryption key must be exactly 32 bytes and the HMAC
  key 32–128 bytes, different from it — what `fwknop --key-gen` produces. fwknop's
  key derivation is a single MD5 round, which is only safe for random keys.
- **Secrets stay put.** Keys live in memory that is zeroed on drop and are never
  logged. The token is removed from the environment before any thread starts. Core
  dumps are off and, on Linux, other processes of the same user cannot read
  ipv6rd's memory or `/proc/<pid>/environ`. `op` runs with a cleared environment,
  must not be writable by anyone but its owner, and is killed after 30 s
  ([ADR 0007](./docs/adr/0007-keys-via-op-cli.md)).
- **Sandboxed service.** The systemd unit runs with no capabilities, a read-only
  filesystem, a syscall filter and restricted address families.

These protections assume the server's access stanza asks for the same things; see
[Server-side settings](#server-side-settings).

## Build

```sh
make build        # or: cargo build --release --locked
```

`make help` lists all targets (build, install-systemd, package, test, interop,
audit, …).

## Configure

All settings live in a TOML file (default `/etc/ipv6rd/ipv6rd.toml` on Linux,
`~/.config/ipv6rd/ipv6rd.toml` on macOS). Copy
[`ipv6rd.toml.example`](./ipv6rd.toml.example) and edit it:

```toml
# Watcher tunables (all optional; defaults shown).
debounce  = "2s"     # quiet period after an address event before recomputing
reconcile = "5m"     # reconciliation poll interval; backstop for missed events
refresh   = "0s"     # grant refresh: re-knock the unchanged address this often; "0" disables
timeout   = "60s"    # per-attempt timeout for a re-knock
retries   = 3        # max attempts before giving up (>= 1)
# op_path = "/usr/bin/op"   # optional; default: `op` on PATH

[fwknop]
dest     = "spa.example.com"        # SPA target (also the probe destination)
port     = 62159                    # UDP port fwknopd listens on
access   = ["tcp/443", "tcp/22"]    # access request; optional, default ["tcp/443"]
key_ref  = "op://fwknop/fwknop key/add more/text"    # base64 encryption key
hmac_ref = "op://fwknop/fwknop hmac/add more/text"   # base64 HMAC key
```

Notes:

- **Durations** use Go syntax (`"300ms"`, `"2s"`, `"5m"`, `"1h30m"`, `"1.5h"`), so
  config files written for the Go version keep working.
- **`refresh`** re-knocks the *unchanged* egress address on a fixed interval, so a
  grant that `fwknopd` expires (`access_timeout` in its access stanza) is renewed
  before it lapses. Without it, a knock is only sent when the address rotates —
  which never happens on a host with a stable address (e.g. a server), so the grant
  is knocked once per daemon start and then left to expire. Set it comfortably
  below the stanza's `access_timeout`; `"0"` (the default) disables it.
- **`fwknop.dest`** is the host running `fwknopd`. It is looked up once at startup
  and must have an AAAA record (or be an IPv6 literal); an IPv4-only target is not
  supported. Restart ipv6rd if its address changes.
- **`fwknop.access`** entries are each `tcp/PORT` or `udp/PORT`, and each is sent as
  its own SPA packet. Port ranges are rejected: an SPA message carries a single
  port. An empty list `[]` is an error; omit the key to default to `["tcp/443"]`.
- **`key_ref` / `hmac_ref`** are 1Password secret references
  (`op://vault/item/field`) for the base64-encoded keys, as printed by
  `fwknop --key-gen`. The encryption key must decode to exactly 32 bytes and the
  HMAC key to 32–128 bytes; the two must differ.
- **`op_path`** names the 1Password CLI. Without it, `op` is looked up on `PATH`
  (absolute directories only) and the path found is logged at startup. Either way,
  ipv6rd refuses an `op` that is group- or world-writable or owned by anyone other
  than root or the user ipv6rd runs as.
- Unknown keys are rejected at startup, so a typo fails loudly rather than being
  silently ignored.

The 1Password **service-account token** is *not* in the file — it is read from the
`OP_SERVICE_ACCOUNT_TOKEN` environment variable.

Config changes take effect on **restart** (`systemctl restart ipv6rd` on Linux,
`launchctl kickstart -k gui/$(id -u)/com.ipv6rd.ipv6rd` on macOS); the file is
not watched.

## Server-side settings

ipv6rd's protections only hold if the SPA target's access stanza requires the same
things. In fwknop-rs's `access.yaml`:

```yaml
- source: ANY
  open_ports: [tcp/443, tcp/22]      # match ipv6rd's fwknop.access
  key_base64: "..."                  # the key behind fwknop.key_ref
  hmac_key_base64: "..."             # the key behind fwknop.hmac_ref; REQUIRED —
                                     # a stanza without one accepts unauthenticated packets
  hmac_digest_type: sha256
  encryption_mode: cbc
  require_source_address: true       # drop packets whose source is not the allow IP
  access_timeout: 30                 # keep ipv6rd's `refresh` comfortably below this
  # require_username: ubuntu         # optional: the user ipv6rd runs as
```

The username in each packet is the login name of the user ipv6rd runs as (`User=`
in the systemd unit).

## Run

```sh
export OP_SERVICE_ACCOUNT_TOKEN=ops_...
ipv6rd                          # reads the default config path
ipv6rd --config ./ipv6rd.toml   # or point at another file
```

`--config` is the only flag; a missing config file is a fatal error.

Exit codes:

| Code | Meaning | Restarted by the systemd unit? |
|---|---|---|
| 0 | Stopped by SIGINT/SIGTERM | — |
| 1 | Runtime failure, e.g. `op` could not reach 1Password | Yes, with a delay growing from 5 s to 15 min |
| 2 | Misconfiguration: bad config, missing token, `op` missing or unsafe, keys that break the key rules | No — fix it and restart by hand |

Restarts are limited because every start spends 1Password service-account requests
(each `op read` counts as 3 against an hourly limit shared by everyone using that
service account).

## Install as a systemd service

The simplest path uses the Makefile:

```sh
make build                  # needs Rust; produces ./ipv6rd
sudo make install-systemd   # install binary + unit + config + 0600 env-file scaffold
sudo systemctl daemon-reload
sudo systemctl enable --now ipv6rd
journalctl -u ipv6rd -f
```

`install-systemd` installs the binary to `/usr/local/bin` (override `PREFIX=`),
rewrites the unit's `ExecStart`/`User` to match, scaffolds `/etc/ipv6rd/ipv6rd.toml`
from `ipv6rd.toml.example` (only if absent, `0644`), and creates
`/etc/ipv6rd/ipv6rd.env` (`0600`, only if absent). It does **not** compile anything,
so the target host needs `make` and root but not the Rust toolchain. To deploy where
Rust isn't installed, build a release tarball on a machine that has it and unpack it
on the target — it bundles the prebuilt binary, the example config, and the Makefile:

```sh
make package                          # on a build host (with Rust) -> dist/ipv6rd-*.tar.gz
# copy the tarball to the target, then there:
tar xzf ipv6rd-*.tar.gz && cd ipv6rd-*/
sudo make install-systemd             # installs the bundled binary; no Rust needed
```

After installing, edit the two files and start the service:

```sh
sudo $EDITOR /etc/ipv6rd/ipv6rd.toml         # SPA target, port, access list, op:// key refs
sudo tee /etc/ipv6rd/ipv6rd.env >/dev/null <<'EOF'
OP_SERVICE_ACCOUNT_TOKEN=ops_your_service_account_token_here
EOF
sudo systemctl restart ipv6rd
```

The unit is sandboxed (`systemd-analyze security ipv6rd` rates it about 1.5, "OK"):
no capabilities, a read-only filesystem except a private `/run/ipv6rd` that serves
as `op`'s config directory, home directories read-only, a syscall filter, and only
the IPv4, IPv6, netlink and Unix address families (`op` may reach 1Password over
IPv4).

`/etc/ipv6rd/ipv6rd.toml` is world-readable: it holds no secret (the token stays in
the `0600` env file), though it does expose the SPA port and `op://` references to
local users.

`make uninstall-systemd` stops the service and removes the unit and binary, leaving
`/etc/ipv6rd/ipv6rd.toml` and `/etc/ipv6rd/ipv6rd.env` (which holds the secret) in
place.

## Install as a launchd user agent (macOS)

No root required. The agent starts at login and runs as your user.

```sh
make build                  # needs Rust; produces ./ipv6rd
make install-launchd        # install binary, wrapper, plist, config, and env-file scaffold
```

`install-launchd` installs the binary to `~/.local/bin` (override `MACOS_PREFIX=`)
and writes a wrapper script, `~/.local/bin/ipv6rd-wrapper`. The wrapper sets
`umask 077`, adds `/opt/homebrew/bin:/usr/local/bin` to `PATH` so `op` is found,
exports the variables in the env file, and execs the binary. The target also
scaffolds `~/.config/ipv6rd/ipv6rd.toml` from `ipv6rd.toml.example` (only if
absent) and creates `~/.config/ipv6rd/ipv6rd.env` (`0600`, only if absent). The
plist restarts the agent at most once every 5 minutes.

After installing, fill in the two files and start the agent:

```sh
$EDITOR ~/.config/ipv6rd/ipv6rd.toml         # SPA target, port, access list, op:// key refs
$EDITOR ~/.config/ipv6rd/ipv6rd.env          # set OP_SERVICE_ACCOUNT_TOKEN=ops_...
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.ipv6rd.ipv6rd.plist
```

Logs go to `~/Library/Logs/ipv6rd.log`.

```sh
tail -f ~/Library/Logs/ipv6rd.log
```

To restart after a config change:

```sh
launchctl kickstart -k gui/$(id -u)/com.ipv6rd.ipv6rd
```

`make uninstall-launchd` unloads the agent and removes the plist, wrapper, and
binary, leaving `~/.config/ipv6rd/ipv6rd.toml` and `~/.config/ipv6rd/ipv6rd.env` in
place.

## Upgrading from the Go version

The Rust version reads the same config file and env file, so for most installs
the upgrade is:

1. Install the 1Password CLI (`op`) on the host. The Go version used the
   1Password Go SDK and needed no CLI.
2. `make build && sudo make install-systemd` (or `make install-launchd`). This
   replaces the unit or plist with the hardened version and leaves your config and
   env file alone.
3. `sudo systemctl daemon-reload && sudo systemctl restart ipv6rd`.

Things that now stop startup (exit code 2) instead of being accepted:

- Keys that are not a 32-byte encryption key plus a different 32–128 byte HMAC key.
  Regenerate them with `fwknop --key-gen`, and update both 1Password and the
  server's access stanza.
- Port ranges in `fwknop.access`, such as `tcp/1000-2000`. fwknop-rs cannot carry
  them in an SPA message, so they never worked end to end. List each port instead.

On macOS, the old wrapper sourced the env file without exporting its variables,
so a plain `OP_SERVICE_ACCOUNT_TOKEN=...` line never reached the binary. The new
wrapper exports them.

## Troubleshooting

`ipv6rd` logs to **stderr** as structured text (one `key=value` line per event,
times in UTC). A healthy startup looks roughly like this:

```
time=… level=INFO msg=starting spa_target=spa.example.com dest_ip=2001:db8::1 spa_port=62159 access="[tcp/443 tcp/22]" …
time=… level=INFO msg="using the 1Password CLI" path=/usr/bin/op
time=… level=INFO msg="resolved fwknop keys from 1Password" key_ref=op://… hmac_ref=op://…
time=… level=INFO msg="egress address changed" reason=startup addr=2001:db8::abcd previous=(none)
time=… level=INFO msg=firing addr=2001:db8::abcd
time=… level=INFO msg="sending access" entry=tcp/443 addr=2001:db8::abcd
time=… level=INFO msg="sending access" entry=tcp/22 addr=2001:db8::abcd
time=… level=INFO msg="fire succeeded" addr=2001:db8::abcd
```

Lines to watch for when something is wrong:

- `msg=exiting err="OP_SERVICE_ACCOUNT_TOKEN is not set"` — the env file is missing,
  blank, or not being loaded (exit code 2).
- `msg=exiting` with `was not found on PATH` or `refusing to run it` in `err` —
  install `op`, set `op_path`, or fix the binary's ownership/permissions (exit
  code 2).
- `msg=exiting err="resolving encryption key \"op://…\": op read …: exit status: 1: …"`
  — `op` ran but failed; the text after `exit status: 1:` is `op`'s own error (bad
  token, wrong reference, no network). Exit code 1, so the service retries with
  backoff.
- `msg=exiting err="the encryption key (fwknop.key_ref) is N bytes…"` — the keys
  break the key rules; regenerate them (exit code 2).
- `msg="egress lookup failed"` / `msg="no egress source address (no IPv6
  connectivity?)"` — the host can't pick a source address for `fwknop.dest`. Check
  that the destination has an AAAA record and the host has working IPv6 (see below).
- `msg="attempt failed" … err="access tcp/443: binding to [2001:db8::abcd]: …"` —
  the egress address disappeared between the probe and the send; the next address
  event or reconcile knocks the new one.
- `msg="fire failed; will retry on next rotation or reconcile"` — every attempt
  failed. The preceding `attempt failed` lines carry the underlying error.
- A knock that ipv6rd reports as sent but the server never acts on: check the
  server's log. `Source address mismatch` means something between the hosts
  rewrites the IPv6 source (e.g. NPTv6); `Username mismatch` means
  `require_username` does not match the user ipv6rd runs as; a timestamp error
  means one of the clocks is off by more than the server's `max_spa_packet_age`
  (120 s by default).

### macOS (launchd)

**View the logs.** launchd redirects both stdout and stderr to a single file:

```sh
tail -f ~/Library/Logs/ipv6rd.log
```

If that file is missing or empty, the agent almost certainly never started — check
its status rather than the log.

**Check whether the agent is loaded and its last exit code:**

```sh
launchctl print gui/$(id -u)/com.ipv6rd.ipv6rd
```

The output's `state` (should be `running`), `pid`, and `last exit code` are the
useful fields. A quicker one-liner:

```sh
launchctl list | grep ipv6rd      # columns: PID  last-exit-status  label
```

A `-` in the PID column with a non-zero exit status means it ran and exited with
an error; launchd retries it at most every 5 minutes.

**Common failures:**

- **`Bootstrap failed: 5: Input/output error` on `launchctl bootstrap`** — the agent
  is already loaded. Boot it out first, then bootstrap again:
  ```sh
  launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.ipv6rd.ipv6rd.plist
  launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.ipv6rd.ipv6rd.plist
  ```
- **Exit code 78 / repeated restarts with an empty log** — launchd couldn't exec
  the wrapper, usually a bad path or missing exec bit. Confirm the wrapper exists
  and is executable, and that the plist points at it:
  ```sh
  ls -l ~/.local/bin/ipv6rd-wrapper        # should be -rwxr-xr-x
  ~/.local/bin/ipv6rd-wrapper              # run it by hand to see the real error
  ```
- **`OP_SERVICE_ACCOUNT_TOKEN` errors / 1Password auth failures** — the wrapper
  exports the variables in `~/.config/ipv6rd/ipv6rd.env` before exec'ing the
  binary; if the token is blank or the file is missing, `op` can't fetch the keys.
  Verify:
  ```sh
  grep OP_SERVICE_ACCOUNT_TOKEN ~/.config/ipv6rd/ipv6rd.env
  ```
- **`op` not found** — the wrapper adds `/opt/homebrew/bin` and `/usr/local/bin` to
  `PATH`. If `op` lives elsewhere, set `op_path` in the config.
- **A "would like to access data from other apps" prompt on every start** — a
  reported `op` behaviour under launchd. ipv6rd runs `op` with
  `OP_LOAD_DESKTOP_APP_SETTINGS=false`, the reported workaround.

**Apply changes and restart.** A config or env-file edit takes effect on restart;
editing the plist requires a full bootout/bootstrap (launchd caches the loaded copy):

```sh
launchctl kickstart -k gui/$(id -u)/com.ipv6rd.ipv6rd   # after a config/env edit
```

### Linux (systemd)

**View the logs:**

```sh
journalctl -u ipv6rd -f             # follow live
journalctl -u ipv6rd -e             # jump to the end
journalctl -u ipv6rd --since "10 min ago"
```

**Check service status and the last exit reason:**

```sh
systemctl status ipv6rd
```

Look at `Active:` (should be `active (running)`) and the most recent log lines it
prints. `status=2/INVALIDARGUMENT` means a misconfiguration: the unit will not
restart it until you fix the problem and run `systemctl restart ipv6rd`.
`status=1/FAILURE` with `activating (auto-restart)` means it is retrying with
backoff — the journal has the error.

**Common failures:**

- **`status=203/EXEC` or `changed working dir … No such file`** — the `ExecStart`
  path is wrong. `install-systemd` rewrites it to `$PREFIX/bin/ipv6rd`; confirm the
  binary is actually there and the unit matches:
  ```sh
  systemctl cat ipv6rd | grep ExecStart
  ```
- **`OP_SERVICE_ACCOUNT_TOKEN` unset / 1Password auth failures** — the token comes
  from `EnvironmentFile=/etc/ipv6rd/ipv6rd.env`. If it's blank the keys can't be
  fetched at startup. Check (as root, since the file is `0600`):
  ```sh
  sudo grep OP_SERVICE_ACCOUNT_TOKEN /etc/ipv6rd/ipv6rd.env
  ```
- **`op` fails only under the service** — `op` runs inside the unit's sandbox,
  with `OP_CONFIG_DIR=/run/ipv6rd`. To check the token and a reference as the
  service user, outside the sandbox, without putting the token on a command line
  or printing the key:
  ```sh
  sudo systemd-run --pipe --wait --uid=ubuntu \
    -p EnvironmentFile=/etc/ipv6rd/ipv6rd.env -E OP_CACHE=false \
    "$(command -v op)" read --no-newline 'op://…' >/dev/null && echo OK
  ```
- **`no egress source address (no IPv6 connectivity?)`** — confirm the host can
  route to the SPA target over IPv6 and that the source it picks is what `fwknopd`
  expects:
  ```sh
  ip -6 route get $(getent ahostsv6 spa.example.com | awk 'NR==1{print $1}')
  ```

**Apply changes and restart.** A config or env edit takes effect on restart; a unit
edit needs a `daemon-reload` first:

```sh
sudo systemctl restart ipv6rd                 # after a config/env edit
sudo systemctl daemon-reload && sudo systemctl restart ipv6rd   # after a unit edit
```

## Development

```sh
make test       # unit tests
make interop    # end-to-end against fwknop-rs's fwknopd (Linux; builds it first)
make vet        # clippy, warnings as errors
make audit      # cargo deny: advisories, licenses, sources
make fmt
```

`make interop` builds `fwknopd` from the same fwknop-rs commit `fkospa` is pinned
to in `Cargo.toml`, runs it unprivileged on `[::1]` in test mode with the stanza
above, and checks that it accepts ipv6rd's knocks and rejects replays and spoofed
sources. CI runs all of these on Linux and the unit tests on macOS.

Bumping the fwknop-rs commit is a deliberate change: review the diff of `fkospa`
first ([ADR 0008](./docs/adr/0008-spa-encoding-via-fkospa.md)).

## License

GPL-2.0-only (see [`LICENSE`](./LICENSE)). The binary links fwknop-rs's `fkospa`,
which is GPL-2.0-only.
