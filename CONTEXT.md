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
Re-sending the fwknop Single Packet Authorization with the current egress address, so the remote firewall re-authorizes the new source IP. The watcher's action on every rotation, performed in-process.
_Avoid_: knock, ping, poke

**Access request**:
The set of protocol/port combinations a re-knock asks `fwknopd` to open for the egress address, e.g. `tcp/443` plus `tcp/22`. Each entry is sent as its own SPA packet to the one SPA target, because the SPA message format carries a single protocol/port.
_Avoid_: access spec (the wire-format string is a serialization of this), port list (omits the protocol)

**SPA target**:
The host running `fwknopd` that a re-knock's SPA packets are sent to. This same host, at the same IPv6 address, doubles as the probe destination, so the egress address selected is exactly the source that host will see — which is why there is no separate destination to configure.
_Avoid_: fwknop host, fwknop server, server

**Allow IP**:
The address an SPA packet asks `fwknopd` to authorize, carried inside the encrypted packet. ipv6rd always sets it to the egress address, which is also the packet's source address.
_Avoid_: source IP (the packet's UDP source is a separate field that merely equals it), allowed address

**Probe destination**:
The destination the kernel runs source-address selection against to determine the egress address; no packet is ever sent to it (a UDP socket is dialed to trigger source-address selection, then immediately closed — see [ADR 0001](./docs/adr/0001-egress-address-via-route-get.md)). It is always the SPA target, never configured independently.
_Avoid_: dest, target (collides with SPA target)

**Grant refresh**:
Re-knocking the current egress address on a fixed interval even though no rotation occurred, so a server-side grant that `fwknopd` expires (its access timeout: `access_timeout` in fwknop-rs) is renewed before it lapses. Optional (`refresh` in the TOML file; disabled by default) and essential on hosts whose egress address is stable (e.g. servers), where rotations alone would never trigger a re-knock after the startup one. The interval measures from the last successful re-knock, whatever triggered it.
_Avoid_: keep-alive, heartbeat, rotation (no address change is involved)
