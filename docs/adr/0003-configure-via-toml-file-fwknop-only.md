# Configure via a TOML file; re-knock fwknop only

ipv6rd now reads all of its configuration from a single TOML file (default
`/etc/ipv6rd/ipv6rd.toml`, overridable with `--config`); the only input from outside the
file is the `OP_SERVICE_ACCOUNT_TOKEN` environment variable. The CLI flags are
gone, and with them the generic `-- command` mode — the tool is now exclusively a
built-in fwknop re-knocker. The fwknop access request is expressed as a list of
protocol/port combinations (`access = ["tcp/443", "tcp/22"]`) that are joined into
one combined access spec and sent in a single SPA packet to one SPA target.
**Updated by `a82d3a7`:** each entry is now sent as its own SPA packet, because the
SPA message format carries a single protocol/port; the rest of this ADR still stands.

We did this because the configuration had outgrown flags — a list of access
entries alongside the watcher tunables and SPA parameters reads far better as a
commented file than as a long `ExecStart` line — and because, once the knock is
performed in-process (ADR-0002), generic command mode had no remaining use here:
the only command it ever ran was the fwknop CLI that ADR-0002 replaced. Dropping it
removes a whole mode, the mode mutual-exclusion rules, the `exec` action, and
`poke-hole.sh`.

## Consequences

- This **supersedes the "generic `-- command` mode is retained" decision in
  ADR-0002**. `poke-hole.sh` is removed.
- With only one mode there is no separately configurable probe destination:
  source-address selection runs against the SPA target itself. An `fwknopd` host
  reachable only over IPv4 is therefore unsupported — we cannot probe it for an
  IPv6 source. See "Probe destination" in `CONTEXT.md`.
- Breaking change for existing deployments: the flag-based `ExecStart` must be
  replaced and a `/etc/ipv6rd/ipv6rd.toml` written. `make install-systemd` rewrites the
  unit and scaffolds the config from `ipv6rd.toml.example` only if it is absent.
- Unknown keys are rejected at load (strict decoding) so typos fail loudly at
  startup. The file is world-readable (`0644`) and holds no secret — the token
  stays in the `0600` env file — but it does expose the SPA port and the `op://`
  references to local users (accepted).
- Adding a non-fwknop authorizer later would mean re-introducing an action seam
  (e.g. an exec mode), not just a config change.

## Considered and rejected

- **Keep flags; just make `--fwknop-access` repeatable.** Rejected: solves only the
  list and leaves a dozen knobs on the command line; the file unifies everything in
  one documented place.
- **Keep generic command mode alongside fwknop.** Rejected: no remaining use once
  the knock is in-process (ADR-0002), and it carries mode mutual-exclusion logic and
  an `exec` surface for no benefit. Re-addable if a non-fwknop authorizer appears.
- **YAML or JSON instead of TOML.** Rejected: JSON has no comments and is unpleasant
  to hand-edit; YAML is whitespace-sensitive with a sharper-edged parser. TOML is the
  idiomatic, comment-friendly choice for a daemon config.
- **Live config reload (file-watch / `SIGHUP`).** Rejected: the config changes rarely
  and is hand-edited; restart-only matches the resolve-keys-once-at-startup model
  (ADR-0002) and fails loudly on a bad edit rather than reloading into a broken state.
