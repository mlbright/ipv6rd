# Determine the egress address via kernel route-lookup source selection

The watcher derives "the current egress address" by asking the kernel which source
address it would select for a destination (`RTM_GETROUTE` via netlink `RouteGet`,
equivalently `ip -6 route get <dest>`), rather than by reading and filtering the
interface address table (`ip -6 addr` / `AddrList`).

This delegates RFC 6724 source-address selection to the kernel, which makes the
result correct when several global addresses coexist during a privacy-address
rotation, and means we never select a `tentative` (DAD-in-progress) or `deprecated`
address. When `<dest>` is pointed at the SPA target, the returned source is exactly
the IP that target will see on the incoming packet — the most correct possible value
for the fwknop re-knock.

## Consequences

- The tool is intentionally agnostic to the `temporary` (RFC 8981) vs `dynamic`
  (DHCPv6) distinction — it never inspects that flag. See "Egress address" in
  `CONTEXT.md`.
- It is Linux/`rtnetlink`-only. A different OS (macOS, BSD) would need a different
  source-selection mechanism.
- `RouteGet` returns the *current* selection only; detecting *change* still requires
  recomputing on netlink address events plus a reconciliation poll.

## Considered and rejected

Parsing `ip -6 addr` / `AddrList`, filtering by scope and flags, and choosing among
multiple preferred global addresses by hand. Rejected: it reimplements source
selection, is more code, and is subtly wrong during the multi-address overlap window
of a rotation.
