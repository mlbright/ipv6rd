# SPA security posture: fixed scheme, one pinned target address

The only exchange between ipv6rd and the SPA target is one-way UDP SPA packets. We
defend that exchange against:

- (a) a passive network observer learning the access request or allow IP;
- (b) an active network attacker forging a knock, replaying one to gain access for
  an IP of their choosing, or forcing a weaker scheme;
- (c) an attacker who can change DNS answers redirecting knocks in a way that
  matters;
- (d) other local, non-root users on the client reading the fwknop keys or the
  1Password token (via logs, `/proc`, core dumps or files).

Out of scope: compromise of the SPA target, of the 1Password account, or of root on
the client. The 1Password fetch is in scope only for (d); its transport is
1Password's own TLS.

Two decisions follow.

**One scheme, not configurable.** ipv6rd sends only Rijndael (AES) in CBC mode with
an HMAC-SHA256 over the ciphertext (encrypt-then-MAC). That is the stock fwknop
default and what the Go version already sent. GPG mode, the legacy IV mode, ECB, CFB,
OFB, CTR and HMAC-less packets are never sent, and there is no setting that could
select them.

**One lookup, one address.** `fwknop.dest` is resolved once at startup to a single
IPv6 address. That address is the probe destination and the destination of every
SPA packet, which goes out on an IPv6 socket with no IPv4 fallback and no further
lookup. A change to the target's AAAA record needs a restart, like any config
change. The Go version resolved the name once for the probe, then let the fwknop
library resolve it again on every send over `"udp"`. A packet could therefore go to
a different address than the probe used, or over IPv4.

**Sent from the address it authorizes.** Each SPA packet goes out on a UDP socket
bound to the egress address and connected to the pinned target address, so the
packet's source always equals its allow IP. fwknop-rs's `require_source_address`
drops any packet where the two differ. If the egress address has vanished since the
probe, the bind fails instead of sending a packet that would be dropped, and the
normal retry and converge-to-latest logic takes over.

**Random keys only.** fwknop derives the AES key with a single MD5 round and no
stretching, which is only safe for random keys. Startup fails unless the encryption
key decodes to exactly 32 bytes (what `fwknop --key-gen` produces) and the HMAC key
to 32–128 bytes, different from the encryption key.

**Secrets stay out of leak paths (threat d).** Keys live only in types that zero
their memory on drop and never print their contents. The 1Password token is read
once, removed from the process environment before any thread starts (so no child
inherits it and the process no longer sees it), and zeroed after use. The kernel's
copy of the initial environment, `/proc/<pid>/environ`, still holds the original
bytes; with the dumpable flag cleared, only root can read it. Core dumps are disabled (`RLIMIT_CORE=0`; on Linux also
`PR_SET_DUMPABLE=0`, which also blocks same-user ptrace and `/proc/<pid>/environ`
and `/proc/<pid>/mem`). Key memory is not mlocked: it is about 64 bytes, and mlock
adds startup failure modes for little gain. The systemd unit carries a sandboxing
baseline checked with `systemd-analyze security`. launchd has no equivalent, so the
macOS wrapper only sets `umask 077`.

**The server must match.** These protections assume the SPA target's access stanza
requires the same things: an HMAC key, `hmac_digest_type: sha256`,
`encryption_mode: cbc` and `require_source_address: true`. A fwknop-rs stanza without
an HMAC key accepts unauthenticated packets. The README documents the expected
stanza.

## Considered and rejected

- **A configurable HMAC digest (e.g. SHA-512).** Rejected: HMAC-SHA256 is not the
  weak point, and a knob is a downgrade surface and a server-side change for no real
  gain.
- **Re-resolve the target before every knock.** Rejected: it would follow DNS changes
  without a restart, but gives a DNS attacker a fresh chance on every knock.
