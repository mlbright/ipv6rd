# IPv6 Egress Address Watcher

A tool that watches the host's outbound IPv6 address and re-knocks an fwknop Single Packet Authorization grant whenever that address changes, so the grant stays pinned to the current source address. All behaviour is driven by a TOML configuration file; the only input from outside the file is the 1Password service-account token.

## Language

**Egress address**:
The preferred global IPv6 address the host uses as the source for new outbound connections. May be an RFC 8981 privacy *temporary* address or a DHCPv6-assigned *dynamic* address — the watcher treats both the same and does not depend on the `temporary` flag.
_Avoid_: temporary private address, temp address (too narrow — implies RFC 8981 only), public IP

**Rotation**:
The event of the egress address value changing from one address to a different one. Lifetime refreshes that keep the same address value are not rotations.
_Avoid_: renewal, refresh, change

**Re-knock**:
Re-sending the fwknop Single Packet Authorization with the current egress address, so the remote firewall re-authorizes the new source IP. The watcher's action on every rotation, performed in-process via the fwknop library.
_Avoid_: knock, ping, poke

**Access request**:
The set of protocol/port combinations a single SPA packet asks `fwknopd` to open for the egress address, e.g. `tcp/443` plus `tcp/22`. Expressed as a list but sent as one combined fwknop access specification in one knock to one SPA target — not one knock per entry.
_Avoid_: access spec (the wire-format string is a serialization of this), port list (omits the protocol)

**SPA target**:
The host running `fwknopd` that a re-knock's SPA packet is sent to. This same host doubles as the probe destination, so the egress address selected is exactly the source that host will see — which is why there is no separate destination to configure.
_Avoid_: fwknop host, server

**Probe destination**:
The destination the kernel runs source-address selection against to determine the egress address; no packet is ever sent to it (it is only an argument to a route lookup — see [ADR 0001](./docs/adr/0001-egress-address-via-route-get.md)). It is always the SPA target, never configured independently.
_Avoid_: dest, target (collides with SPA target)
