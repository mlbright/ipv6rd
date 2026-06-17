# ipv6rd

Watches the host's **egress IPv6 address** and runs a command whenever that
address rotates, passing the new address as the final argument. Its purpose is to
keep an [fwknop](https://github.com/mrash/fwknop) Single Packet Authorization grant
in sync with a rotating IPv6 address, so you don't get locked out when your source
IP changes — but the command is generic.

See [`CONTEXT.md`](./CONTEXT.md) for terminology and
[`docs/adr/`](./docs/adr/) for design decisions.

## How it works

1. Subscribes to kernel address events over netlink (`AddrSubscribe`).
2. On each event (debounced ~2s) — and on a periodic reconciliation poll — it asks
   the kernel which source address it would use to reach `--dest`
   (`RouteGet`, i.e. `ip -6 route get`). That `src` is the egress address.
   See [ADR 0001](./docs/adr/0001-egress-address-via-route-get.md).
3. If the egress address differs from the last one it acted on, it runs the command
   with the new address appended as the last argument (and exported as
   `$IPV6RD_ADDR`).

It fires once on startup, serializes runs (converging to the latest address if it
rotates again mid-run), and retries a failed command with bounded exponential
backoff (2s → 8s → 32s) before waiting for the next event. State is in memory only.

**Linux-only** (uses `rtnetlink`). Runs unprivileged.

## Build

```sh
go build -o ipv6rd .
```

## Usage

```sh
ipv6rd [flags] -- command [args...]
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--dest` | `2606:4700:4700::1111` | IPv6 address or hostname used for source-address selection. For maximum fwknop precision, point it at the SPA target so the source returned is exactly what that server sees. |
| `--debounce` | `2s` | Quiet period after an address event before recomputing. |
| `--reconcile` | `5m` | Reconciliation poll interval; backstop for missed netlink events. |
| `--timeout` | `60s` | Per-attempt command timeout. |
| `--retries` | `3` | Max command attempts before giving up. |

Example (the fwknop driver):

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

The unit runs as `User=mbright` so the 1Password CLI (`op`) session used by
`poke-hole.sh` is available.
