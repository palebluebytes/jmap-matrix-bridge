# Configuration

Every flag has an environment-variable equivalent (shown in parentheses). Run
`jmap-matrix-bridge run --help` for the authoritative list — this page explains
the ones that need more than a line.

## Flags

| Flag (env) | Default | Meaning |
| --- | --- | --- |
| `--jmap-url` (`JMAP_URL`) | *required* | JMAP session/discovery URL |
| `--matrix-url` (`MATRIX_URL`) | *required* | Matrix homeserver Client-Server API URL |
| `--matrix-as-token` (`MATRIX_AS_TOKEN`) | *one of the pair* | Bridge → homeserver auth token |
| `--matrix-as-token-file` (`MATRIX_AS_TOKEN_FILE`) | *one of the pair* | File holding the AS token (keeps it out of `ps`/`/proc`) |
| `--matrix-hs-token` (`MATRIX_HS_TOKEN`) | *one of the pair* | Homeserver → bridge transaction auth token |
| `--matrix-hs-token-file` (`MATRIX_HS_TOKEN_FILE`) | *one of the pair* | File holding the hs_token |
| `--matrix-domain` (`MATRIX_DOMAIN`) | `localhost` | Matrix server name (used to build ghost mxids, and the default permission grant) |
| `--listen-address` (`LISTEN_ADDRESS`) | `127.0.0.1` | Address to bind. Use `0.0.0.0` for container/multi-host setups where the homeserver connects from another host |
| `--port` (`PORT`) | `8008` | TCP port the bridge listens on |
| `--db` (`DATABASE_URL`) | `sqlite:bridge.db` | SQLite database URL |
| `--encryption-key` (`ENCRYPTION_KEY`) | — | AES-256 key (base64 or hex) for credentials at rest |
| `--encryption-key-file` (`ENCRYPTION_KEY_FILE`) | — | File holding the AES-256 key (preferred over inline) |
| `--double-puppet-secret` (`DOUBLE_PUPPET_SECRET`) | — | Shared secret enabling automatic double-puppeting |
| `--double-puppet-secret-file` (`DOUBLE_PUPPET_SECRET_FILE`) | — | File holding that secret (preferred) |
| `--render-mode` (`RENDER_MODE`) | `links` | Email body rendering: `plain`, `links`, or `rich` |
| `QUOTE_REPLIES` | `true` | Quote the parent in outbound replies (email-only, never shown in Matrix). **Env var only — see [Switches](#switches-not-value-taking-flags)** |
| `--bridge-mailboxes` (`BRIDGE_MAILBOXES`) | `false` | Also mirror JMAP mailboxes (Inbox/Sent/…) as their own rooms |
| `--jmap-sync-limit` (`JMAP_SYNC_LIMIT`) | `10` | Emails fetched per JMAP query page during sync and backfill |
| `--permission KEY=LEVEL` | *(repeatable)* | Grant bridge access — see [Permissions](#permissions) |
| `--user SPEC` | *(repeatable)* | Declaratively provision a user — see [Declarative provisioning](#declarative-provisioning) |
| `--log-level` (`LOG_LEVEL`) | `info` | `error` \| `warn` \| `info` \| `debug` \| `trace` (global flag) |

The token flags come in pairs: supply **exactly one** of `--matrix-as-token` /
`--matrix-as-token-file` and one of `--matrix-hs-token` /
`--matrix-hs-token-file`. Passing both members of a pair is an error. Prefer the
`-file` forms — an inline token is visible in `ps` and `/proc`.

If no encryption key is given, credentials are stored in plain text (legacy mode).

### Switches, not value-taking flags

`--quote-replies` and `--bridge-mailboxes` are switches: passing
`--bridge-mailboxes` turns mirroring **on**, and `--quote-replies false` is
rejected. Since quoting defaults to on, the only way to turn it **off** is the
environment variable, `QUOTE_REPLIES=false` — which is what the NixOS module does.

## Permissions

Access is **default-deny** ([ADR-0010](adr/0010-permission-model.md)). Each
`--permission` value is `key=level`:

- `key` is a full MXID (`@you:example.com`), a homeserver domain (`example.com`),
  or `*` for everyone. Most specific match wins.
- `level` is `user` (log in, operate their own JMAP account, non-destructive
  commands) or `admin` (that plus destructive and global commands).

When `--permission` is omitted entirely, the bridge grants `user` to its own
`--matrix-domain` and denies everyone else — so a single-homeserver install works
untouched while federated senders are refused. A sender matching no entry cannot
even `login`.

```bash
--permission "example.com=user" --permission "@admin:example.com=admin"
```

## Double-puppeting

Double-puppeting makes mail *you* sent appear authored by your own Matrix account
rather than a ghost. There are two routes:

- **Automatic** ([ADR-0014](adr/0014-automatic-double-puppet-via-shared-secret.md)) —
  set `--double-puppet-secret-file`. An interactive `login` by a local user then
  mints the token itself, with nothing to paste. Requires the homeserver to run a
  shared-secret-auth module.
- **Manual** ([ADR-0002](adr/0002-double-puppet-via-login-token.md)) — the user
  runs `login-matrix <access-token>` with a token from their client. This is the
  fallback when no shared secret is configured, or when the homeserver has no
  shared-secret-auth module.

Read-state sync from JMAP to Matrix requires double-puppeting; without it, marking
mail read in your mail client silently does nothing in Matrix
([ADR-0015](adr/0015-read-state-jmap-to-matrix-requires-double-puppet.md)).

## Declarative provisioning

Instead of (or in addition to) interactive login, users can be provisioned at
startup with one repeatable `--user` flag per user — a comma-separated list of
`key=value` pairs:

```bash
--user "mxid=@you:example.com,username=you@mail.example.com,token-file=/run/secrets/jmap"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `mxid` | yes | Matrix user id, e.g. `@you:example.com` |
| `username` | yes | JMAP username |
| `url` | no | JMAP session URL; defaults to `--jmap-url` |
| `token-file` | preferred | Path to a file holding the JMAP token (never exposed in argv) |
| `token` | alternative | The JMAP token inline (visible in argv) |
| `matrix-password-file` | no | Enables double-puppet auto-accept |

> **Deprecated:** the single-user `--jmap-username` / `--jmap-token` /
> `--jmap-token-file` flags (`JMAP_USERNAME` / `JMAP_TOKEN` / `JMAP_TOKEN_FILE`)
> are legacy. Use interactive `!login` or `--user`.

## NixOS

The NixOS module ([`nix/module/README.md`](../nix/module/README.md)) exposes the
common options directly. Anything it does not model — permissions and the
double-puppet secret among them — is reachable through its `extraArgs` option.
