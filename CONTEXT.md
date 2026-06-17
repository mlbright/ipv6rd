# IPv6 Egress Address Watcher

A tool that watches the host's outbound IPv6 address and runs a configured command whenever that address changes, so that an external authorization pinned to the old address (primarily an fwknop SPA grant) can be refreshed automatically.

## Language

**Egress address**:
The preferred global IPv6 address the host uses as the source for new outbound connections. May be an RFC 8981 privacy *temporary* address or a DHCPv6-assigned *dynamic* address — the watcher treats both the same and does not depend on the `temporary` flag.
_Avoid_: temporary private address, temp address (too narrow — implies RFC 8981 only), public IP

**Rotation**:
The event of the egress address value changing from one address to a different one. Lifetime refreshes that keep the same address value are not rotations.
_Avoid_: renewal, refresh, change

**Re-knock**:
Re-sending the fwknop Single Packet Authorization with the current egress address, so the remote firewall re-authorizes the new source IP. The watcher's primary action on rotation — performed either in-process via the fwknop library (built-in fwknop mode) or by running a configured command (generic mode).
_Avoid_: knock, ping, poke
