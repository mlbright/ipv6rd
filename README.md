# ipv6rd

Watches the host's **egress IPv6 address** and re-knocks an
[fwknop](https://github.com/mrash/fwknop) Single Packet Authorization grant
whenever that address rotates, so you don't get locked out when your source IP
changes. It can do this two ways:

- **Built-in fwknop mode** — sends the SPA packet itself via the
  [fwknop-go](https://github.com/damienstuart/fwknop-go) library, fetching the
  encryption and HMAC keys from [1Password](https://developer.1password.com/docs/sdks/)
  through a service account. No external `fwknop` binary or `op` CLI required.
- **Generic command mode** — runs any command with the new address appended as
  the final argument (also exported as `$IPV6RD_ADDR`). Useful for custom
  knockers such as [`poke-hole.sh`](./poke-hole.sh).

See [`CONTEXT.md`](./CONTEXT.md) for terminology and
[`docs/adr/`](./docs/adr/) for design decisions.

## How it works

1. Subscribes to kernel address events over netlink (`AddrSubscribe`).
2. On each event (debounced ~2s) — and on a periodic reconciliation poll — it asks
   the kernel which source address it would use to reach `--dest`
   (`RouteGet`, i.e. `ip -6 route get`). That `src` is the egress address.
   See [ADR 0001](./docs/adr/0001-egress-address-via-route-get.md).
3. If the egress address differs from the last one it acted on, it re-knocks: in
   built-in fwknop mode it sends the SPA packet in process; in generic mode it
   runs the command with the new address appended as the last argument (and
   exported as `$IPV6RD_ADDR`).

It fires once on startup, serializes runs (converging to the latest address if it
rotates again mid-run), and retries a failed re-knock with bounded exponential
backoff (2s → 8s → 32s) before waiting for the next event. State is in memory only.
In built-in fwknop mode the 1Password keys are resolved once at startup and cached,
so each rotation only sends the SPA packet.

**Linux-only** (uses `rtnetlink`). Runs unprivileged.

## Build

```sh
go build -o ipv6rd .
```

## Usage

```sh
# Built-in fwknop mode
ipv6rd [flags] --fwknop-dest HOST --fwknop-port N \
  --fwknop-key-ref op://vault/item/field --fwknop-hmac-ref op://vault/item/field

# Generic command mode
ipv6rd [flags] -- command [args...]
```

The two modes are mutually exclusive: setting any `--fwknop-*` flag selects
built-in fwknop mode and a trailing `-- command` is then rejected.

| Flag | Default | Meaning |
|------|---------|---------|
| `--dest` | `2606:4700:4700::1111` | IPv6 address or hostname used for source-address selection. For maximum fwknop precision, point it at the SPA target so the source returned is exactly what that server sees. |
| `--debounce` | `2s` | Quiet period after an address event before recomputing. |
| `--reconcile` | `5m` | Reconciliation poll interval; backstop for missed netlink events. |
| `--timeout` | `60s` | Per-attempt timeout for the re-knock. |
| `--retries` | `3` | Max attempts before giving up. |

Built-in fwknop mode flags:

| Flag | Default | Meaning |
|------|---------|---------|
| `--fwknop-dest` | _(required)_ | SPA target host the packet is sent to. Setting any `--fwknop-*` flag enables this mode. |
| `--fwknop-port` | _(required)_ | UDP port `fwknopd` listens on for SPA packets. |
| `--fwknop-access` | `tcp/443` | Access specification to request. |
| `--fwknop-key-ref` | _(required)_ | 1Password secret reference (`op://vault/item/field`) for the base64 encryption key. |
| `--fwknop-hmac-ref` | _(required)_ | 1Password secret reference for the base64 HMAC key. |

In built-in fwknop mode the 1Password service-account token is read from the
`OP_SERVICE_ACCOUNT_TOKEN` environment variable. The resolved secret values are
base64-decoded to raw key bytes (mirroring `fwknop --key-base64` /
`--key-base64-hmac`).

Example (built-in fwknop mode):

```sh
export OP_SERVICE_ACCOUNT_TOKEN=ops_...
ipv6rd --dest notes.martyloo.com \
  --fwknop-dest notes.martyloo.com --fwknop-port 62159 --fwknop-access tcp/443 \
  --fwknop-key-ref 'op://fwknop/fwknop key/add more/text' \
  --fwknop-hmac-ref 'op://fwknop/fwknop hmac/add more/text'
```

Example (generic command mode):

```sh
ipv6rd -- /home/ubuntu/ipv6rd/poke-hole.sh
```

The command receives the new address as its final argument, so `poke-hole.sh`
reads it as `$1`.

## Install as a systemd service

```sh
go build -o ipv6rd .
sudo cp ipv6rd.service /etc/systemd/system/ipv6rd.service
sudo systemctl daemon-reload
sudo systemctl enable --now ipv6rd
journalctl -u ipv6rd -f
```

The shipped unit runs in built-in fwknop mode and reads the 1Password
service-account token from an `EnvironmentFile`. Create it with `0600`
permissions, owned by the user the service runs as:

```sh
sudo install -m 600 -o mbright /dev/null /etc/ipv6rd.env
sudo tee /etc/ipv6rd.env >/dev/null <<'EOF'
OP_SERVICE_ACCOUNT_TOKEN=ops_your_service_account_token_here
EOF
```

To use generic command mode instead, change `ExecStart` to pass `-- <command>`
(e.g. `poke-hole.sh`) and drop the `--fwknop-*` flags; the `EnvironmentFile` is
only needed for built-in fwknop mode.
