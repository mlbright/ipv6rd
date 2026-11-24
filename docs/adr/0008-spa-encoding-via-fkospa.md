# Build SPA packets with fwknop-rs's `fkospa` crate

ipv6rd builds and encrypts SPA packets with the `fkospa` crate from
[fwknop-rs](https://github.com/mlbright/fwknop-rs), pinned to a git commit. The SPA
target runs fwknop-rs's `fwknopd`, so the client encodes with exactly the code the
server decodes with, and protocol fixes land in one place. `fkospa` already zeroes
keys on drop, redacts them from `Debug`, and runs the libfko known-answer vectors.

ipv6rd uses only `fkospa`, not the `fwknop` crate's `Knock` sender, because that
sender resolves the hostname itself and picks its own socket.
[ADR-0006](./0006-spa-security-posture.md) needs ipv6rd to own the destination and
the source address.

## Consequences

- `fkospa`'s API still offers the legacy IV mode and the MD5 and SHA-1 digests.
  ipv6rd's own code always passes `EncryptionMode::Cbc` and `HmacType::Sha256`, and
  no config path reaches anything else. Tests hold that line, not the type system.
- `fkospa` is GPL-2.0-only, so the distributed ipv6rd binary is GPL-2.0-only.
- `fkospa` is new (v0.1.0, 2026-11) and unaudited. Bumping the pinned commit is a
  deliberate change that gets reviewed.

## Considered and rejected

- **A minimal encoder in this repo that can only do CBC + HMAC-SHA256.** It would
  keep the weak modes out of the binary entirely, and `fkospa` could still check its
  output in tests. Rejected: two copies of the protocol code to keep in step and
  review, for a guarantee ipv6rd's call site and tests already give.
- **The `fwknop` crate's `Knock` sender.** Rejected: it looks the hostname up
  again for every send, the flaw ADR-0006 removes.
- **Bindings to libfko (C).** None exist. libfko also rejects an IPv6 allow IP.
