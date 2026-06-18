# Determine the egress address via kernel source-address selection

The watcher derives "the current egress address" by asking the kernel which source
address it would select for a destination, rather than by reading and filtering the
interface address table (`ip -6 addr` / `AddrList`).

The mechanism is a UDP socket dial: `net.Dial("udp6", dest)` binds a socket without
sending any packets, and the kernel performs RFC 6724 source-address selection as
part of the connect. Reading `conn.LocalAddr()` returns exactly the IP the kernel
would use as the source for real traffic to that destination.

This approach is cross-platform, correct when several global addresses coexist during
a privacy-address rotation, and never selects a `tentative` (DAD-in-progress) or
`deprecated` address. When `dest` is the SPA target, the returned source is exactly
the IP that target will see on the incoming packet — the most correct possible value
for the fwknop re-knock.

## Consequences

- The tool is intentionally agnostic to the `temporary` (RFC 8981) vs `dynamic`
  (DHCPv6) distinction — it never inspects that flag. See "Egress address" in
  `CONTEXT.md`.
- This is a pure-Go, cross-platform implementation with no OS-specific API surface.
  See [ADR-0004](./0004-macos-support.md) for the macOS port that motivated the
  change from `RouteGet`.
- The dial returns the *current* selection only; detecting *change* still requires
  recomputing on address events plus a reconciliation poll.

## Considered and rejected

- **Parsing `ip -6 addr` / `AddrList`**, filtering by scope and flags, and choosing
  among multiple preferred global addresses by hand. Rejected: it reimplements source
  selection, is more code, and is subtly wrong during the multi-address overlap window
  of a rotation.
- **`netlink.RouteGet`** (`RTM_GETROUTE`, equivalently `ip -6 route get <dest>`).
  Used in the original implementation. Rejected when macOS support was added
  (ADR-0004) because it is Linux/`rtnetlink`-only. The `net.Dial` trick delivers
  identical semantics portably.
