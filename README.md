# ipv6rd

Watches the host's **egress IPv6 address** and re-knocks an
[fwknop](https://github.com/mrash/fwknop) Single Packet Authorization grant
whenever that address rotates, so you don't get locked out when your source IP
changes.

It sends the SPA packet itself, in process, via the
[fwknop-go](https://github.com/damienstuart/fwknop-go) library, fetching the
encryption and HMAC keys from [1Password](https://developer.1password.com/docs/sdks/)
through a service account. No external `fwknop` binary or `op` CLI required.

Everything is configured by a single TOML file. The only input from outside the
file is the 1Password service-account token, read from the environment.

See [`CONTEXT.md`](./CONTEXT.md) for terminology and
[`docs/adr/`](./docs/adr/) for design decisions.

## How it works

1. Subscribes to kernel address events over netlink (`AddrSubscribe`).
2. On each event (debounced ~2s) — and on a periodic reconciliation poll — it asks
   the kernel which source address it would use to reach the **SPA target**
   (`RouteGet`, i.e. `ip -6 route get <spa-target>`). That `src` is the egress
   address. Because the probe destination *is* the SPA target, the source returned
   is exactly the IP that target will see. See
   [ADR 0001](./docs/adr/0001-egress-address-via-route-get.md).
3. If the egress address differs from the last one it acted on, it re-knocks:
   it sends one SPA packet authorizing the new address for the configured
   [access request](./CONTEXT.md) (all listed protocol/port combinations in a
   single packet).

It fires once on startup, serializes runs (converging to the latest address if it
rotates again mid-run), and retries a failed re-knock with bounded exponential
backoff (2s → 8s → 32s) before waiting for the next event. State is in memory only.
The 1Password keys are resolved once at startup and cached, so each rotation only
sends the SPA packet. See
[ADR 0002](./docs/adr/0002-in-process-fwknop-knock-via-library-and-1password-sdk.md)
and [ADR 0003](./docs/adr/0003-configure-via-toml-file-fwknop-only.md).

Runs unprivileged on **Linux** (rtnetlink) and **macOS** (poll-based, 5 s interval).

## Build

```sh
make build        # or: go build -o ipv6rd .
```

`make help` lists all targets (build, install, install-systemd, package, …).

## Configure

All settings live in a TOML file (default `/etc/ipv6rd/ipv6rd.toml`). Copy
[`ipv6rd.toml.example`](./ipv6rd.toml.example) and edit it:

```toml
# Watcher tunables (all optional; defaults shown).
debounce  = "2s"     # quiet period after an address event before recomputing
reconcile = "5m"     # reconciliation poll interval; backstop for missed events
timeout   = "60s"    # per-attempt timeout for a re-knock
retries   = 3        # max attempts before giving up (>= 1)

[fwknop]
dest     = "spa.example.com"        # SPA target (also the probe destination)
port     = 62159                    # UDP port fwknopd listens on
access   = ["tcp/443", "tcp/22"]    # access request; optional, default ["tcp/443"]
key_ref  = "op://fwknop/fwknop key/add more/text"    # base64 encryption key
hmac_ref = "op://fwknop/fwknop hmac/add more/text"   # base64 HMAC key
```

Notes:

- **`fwknop.dest`** is the host running `fwknopd`. The packet is sent there, and
  the same host is used for source-address selection — so it must have an AAAA
  record (or be an IPv6 literal). An IPv4-only `fwknopd` host is not supported.
- **`fwknop.access`** entries are each `tcp|udp/port` or `tcp|udp/lo-hi` for a
  range. They are sent as one combined access spec in a single SPA packet. An
  empty list `[]` is an error; omit the key to default to `["tcp/443"]`.
- **`key_ref` / `hmac_ref`** are 1Password secret references
  (`op://vault/item/field`) for the base64-encoded keys. The resolved values are
  base64-decoded to raw key bytes (mirroring `fwknop --key-base64` /
  `--key-base64-hmac`).
- Unknown keys are rejected at startup, so a typo fails loudly rather than being
  silently ignored.

The 1Password **service-account token** is *not* in the file — it is read from the
`OP_SERVICE_ACCOUNT_TOKEN` environment variable.

Config changes take effect on **restart** (`systemctl restart ipv6rd` on Linux,
`launchctl kickstart -k gui/$(id -u) com.ipv6rd.ipv6rd` on macOS); the file is
not watched.

## Run

```sh
export OP_SERVICE_ACCOUNT_TOKEN=ops_...
ipv6rd                          # reads /etc/ipv6rd/ipv6rd.toml
ipv6rd --config ./ipv6rd.toml   # or point at another file
```

`--config` is the only flag; a missing config file is a fatal error.

## Install as a systemd service

The simplest path uses the Makefile:

```sh
make build                  # needs Go; produces ./ipv6rd
sudo make install-systemd   # install binary + unit + config + 0600 env-file scaffold
sudo systemctl daemon-reload
sudo systemctl enable --now ipv6rd
journalctl -u ipv6rd -f
```

`install-systemd` installs the binary to `/usr/local/bin` (override `PREFIX=`),
rewrites the unit's `ExecStart`/`User` to match, scaffolds `/etc/ipv6rd/ipv6rd.toml` from
`ipv6rd.toml.example` (only if absent, `0644`), and creates `/etc/ipv6rd/ipv6rd.env`
(`0600`, only if absent). It does **not** compile anything, so the target host
needs `make` and root but not the Go toolchain. To deploy where Go isn't installed,
build a release tarball on a machine that has Go and unpack it on the target — it
bundles the prebuilt binary, the example config, and the Makefile:

```sh
make package                          # on a build host (with Go) -> dist/ipv6rd-*.tar.gz
# copy the tarball to the target, then there:
tar xzf ipv6rd-*.tar.gz && cd ipv6rd-*/
sudo make install-systemd             # installs the bundled binary; no Go needed
```

After installing, edit the two files and start the service:

```sh
sudo $EDITOR /etc/ipv6rd/ipv6rd.toml         # SPA target, port, access list, op:// key refs
sudo tee /etc/ipv6rd/ipv6rd.env >/dev/null <<'EOF'
OP_SERVICE_ACCOUNT_TOKEN=ops_your_service_account_token_here
EOF
sudo systemctl restart ipv6rd
```

`/etc/ipv6rd/ipv6rd.toml` is world-readable: it holds no secret (the token stays in the
`0600` env file), though it does expose the SPA port and `op://` references to
local users.

`make uninstall-systemd` stops the service and removes the unit and binary, leaving
`/etc/ipv6rd/ipv6rd.toml` and `/etc/ipv6rd/ipv6rd.env` (which holds the secret) in place.

## Install as a launchd user agent (macOS)

No root required. The agent starts at login and runs as your user.

```sh
make build                  # needs Go; produces ./ipv6rd
make install-launchd        # install binary, wrapper, plist, config, and env-file scaffold
```

`install-launchd` installs the binary to `/usr/local/bin` (override `PREFIX=`), writes
a wrapper script at `/usr/local/bin/ipv6rd-wrapper` that sources the env file before
exec-ing the binary, scaffolds `~/.config/ipv6rd/ipv6rd.toml` from `ipv6rd.toml.example`
(only if absent), and creates `~/.config/ipv6rd/ipv6rd.env` (`0600`, only if absent).

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
launchctl kickstart -k gui/$(id -u) com.ipv6rd.ipv6rd
```

`make uninstall-launchd` unloads the agent and removes the plist, wrapper, and binary,
leaving `~/.config/ipv6rd/ipv6rd.toml` and `~/.config/ipv6rd/ipv6rd.env` in place.
