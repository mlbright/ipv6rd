# Re-knock in-process via the fwknop library, with keys from the 1Password SDK

For its primary use case, the watcher now performs the fwknop re-knock *in-process*
by calling the `fwknop-go` library (a high-level `fwknop.Knock{...}.Send(ctx)`
helper that encodes, encrypts, and UDP-sends the SPA), and obtains the encryption
and HMAC keys directly from 1Password via the official Go SDK
(`onepassword-sdk-go`) using a **service-account token**. This replaces the previous
default of shelling out to `poke-hole.sh`, which ran the `fwknop` CLI with keys
piped from the `op` CLI and `jq`.

We did this to remove the `fwknop`, `op`, and `jq` runtime dependencies and the
shell indirection, to fail fast on misconfiguration (a bad token or wrong secret
reference now errors at startup rather than on the first rotation), and to keep the
keys off the process command line. Keys are static, so they are resolved once at
startup and cached for the process lifetime; re-knocks are then pure local
encode + UDP send and never touch 1Password.

## Consequences

- ipv6rd gains hard dependencies on `github.com/damienstuart/fwknop-go` (wired via a
  `replace => ../fwknop-go` directive until that module is tagged/published) and on
  the 1Password Go SDK. `fwknop-go` itself does **not** depend on the 1Password SDK —
  the `fwknop.Knock` helper takes raw key bytes, keeping the library clean. The
  helper lives in the `fwknop` subpackage
  (`github.com/damienstuart/fwknop-go/fwknop`) so its directory name matches its
  package name; the protocol/crypto stays in `fkospa`.
- The deployment model changes: the systemd unit supplies
  `OP_SERVICE_ACCOUNT_TOKEN` via a `0600` `EnvironmentFile` instead of relying on an
  interactive `op` session. Rotating the keys in 1Password requires a service
  restart.
- The generic `-- command` mode is **retained**; `poke-hole.sh` stays as a working
  example of it. Built-in fwknop mode is selected by the `--fwknop-*` flags.
  **Superseded by [ADR-0003](./0003-configure-via-toml-file-fwknop-only.md):** the
  CLI flags and generic command mode were later removed; ipv6rd is now configured by
  a TOML file and re-knocks fwknop only. The rest of this ADR (in-process knock via
  the library, keys from the 1Password SDK, resolved once at startup) still stands.
- **Superseded in part by [ADR-0007](./0007-keys-via-op-cli.md):** the Rust rewrite
  fetches the keys with `op read` instead of the Go SDK; they are still resolved
  once at startup through a service account.

## Considered and rejected

- **Keep shelling out to `poke-hole.sh`.** Rejected: depends on `fwknop`, `op`, and
  `jq` being installed and on a live `op` session, and hides failures behind a shell
  pipeline.
- **Use the `op` CLI from Go (`exec`) instead of the SDK.** Rejected: still an
  external-binary dependency and an interactive-session assumption; the SDK with a
  service account is the supported non-interactive path.
- **Resolve keys on every re-knock.** Rejected: the keys never change, so this only
  adds latency and a new failure mode to the per-rotation hot path.
