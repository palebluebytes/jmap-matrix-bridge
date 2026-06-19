# JMAP Matrix Bridge

A Rust [Matrix Application Service](https://spec.matrix.org/latest/application-service-api/)
that bridges a [JMAP](https://jmap.io/) email account (Stalwart, Fastmail, …) into
Matrix: each email conversation becomes a Matrix room, and messages you send in
Matrix go back out as email.

The vocabulary used throughout this project (ghost, puppet, bot, thread, room,
space, submission, backfill) is defined in [`CONTEXT.md`](CONTEXT.md). The
architectural decisions behind it are recorded in [`docs/adr/`](docs/adr/).

## How it works

- **One room per email thread.** A bridged conversation's Matrix room is scoped to
  a single JMAP email thread, not to a correspondent — a reply within the thread
  returns to the same room; a new thread gets a new room. Rooms are grouped under a
  private **space** named for your own email address.
  ([ADR-0001](docs/adr/0001-custom-jmap-matrix-bridge.md))
- **Ghosts** represent your correspondents as Matrix users in the bridge's
  exclusive `@_jmap_*` namespace. The localpart is your correspondent's address
  with non-alphanumeric characters hex-encoded, e.g. `alice@example.com` →
  `@_jmap_alice=40example.com:your.server`.
- **Double-puppeting** (optional) makes mail *you* sent appear authored by your own
  Matrix account rather than a ghost, via a one-time access token — not a
  declarative `as_token`. ([ADR-0002](docs/adr/0002-double-puppet-via-login-token.md))
- **Push-driven sync.** The bridge subscribes to JMAP EventSource pushes, debounces
  them, and reconciles missed pushes via JMAP state tokens (with an hourly heartbeat
  poll as a backstop) — it does not poll on a fixed interval. Historical mail is
  imported separately, oldest-first, so Element's room list sorts newest-first.
  ([ADR-0005](docs/adr/0005-backfill-oldest-first.md))
- **Verified outbound delivery.** A Matrix→email send is only treated as delivered
  once the JMAP `EmailSubmission` is confirmed; failures go to a durable retry queue
  with exponential backoff and a user-visible give-up notice.
  ([ADR-0007](docs/adr/0007-verified-send-with-retry-queue.md))
- **Single instance.** The bridge is designed to run as exactly one process;
  cross-task coordination lives in the database.
  ([ADR-0006](docs/adr/0006-single-instance-db-coordination.md))

## Architecture

- **`ClientManager`** — owns one JMAP client and one background event loop
  (`run_event_loop`) per bridged Matrix user; loads users from the database on
  startup. Shared per-user state is guarded by `tokio` async locks
  ([ADR-0003](docs/adr/0003-tokio-async-locks-over-dashmap.md)).
- **`Store`** — a local SQLite database holding users (with credentials encrypted at
  rest when an encryption key is configured), thread↔room↔event mappings, the
  outbound retry queue, processed-transaction and destroyed-email dedup tables, and
  per-user sync/backfill state.
- **Axum web server** — serves the Matrix Application Service transaction endpoint,
  authenticated with the `hs_token`.

## Build

```bash
nix build .#jmap-matrix-bridge
# or
cargo build --release
```

## Configure & run

### 1. Generate a registration file

```bash
jmap-matrix-bridge generate-registration \
  --url http://localhost:8008 \
  --output registration.yaml
```

This writes a registration with id `jmap-bridge`, sender localpart `_jmap_bot`
(the bot user, e.g. `@_jmap_bot:your.server`), and the `@_jmap_.*` user namespace.
Load it into your homeserver (for tuwunel, drop it into the `appservice_dir`; for
Synapse/Dendrite, reference it from the homeserver config).

### 2. Run the service

The bridge runs as a daemon — typically via the NixOS module (see
[`nix/module/`](nix/module/) and its [README](nix/module/README.md)). A manual
invocation:

```bash
jmap-matrix-bridge run \
  --jmap-url https://mail.example.com/.well-known/jmap \
  --matrix-url http://localhost:6167 \
  --matrix-as-token "$MATRIX_AS_TOKEN" \
  --matrix-hs-token "$MATRIX_HS_TOKEN" \
  --matrix-domain example.com \
  --port 8008 \
  --db sqlite:bridge.db \
  --encryption-key-file /run/secrets/jmap-bridge-key
```

### Flags

Every flag has an environment-variable equivalent (shown in parentheses). Run
`jmap-matrix-bridge run --help` for the authoritative list.

| Flag (env) | Default | Meaning |
| --- | --- | --- |
| `--jmap-url` (`JMAP_URL`) | *required* | JMAP session/discovery URL |
| `--matrix-url` (`MATRIX_URL`) | *required* | Matrix homeserver Client-Server API URL |
| `--matrix-as-token` (`MATRIX_AS_TOKEN`) | *required* | Bridge → homeserver auth token |
| `--matrix-hs-token` (`MATRIX_HS_TOKEN`) | *required* | Homeserver → bridge transaction auth token |
| `--matrix-domain` (`MATRIX_DOMAIN`) | `localhost` | Matrix server name (used to build ghost mxids) |
| `--port` (`PORT`) | `8008` | TCP port the bridge listens on |
| `--db` (`DATABASE_URL`) | `sqlite:bridge.db` | SQLite database URL |
| `--encryption-key` (`ENCRYPTION_KEY`) | — | AES-256 key (base64 or hex) for credentials at rest |
| `--encryption-key-file` (`ENCRYPTION_KEY_FILE`) | — | File holding the AES-256 key (preferred over inline) |
| `--render-mode` (`RENDER_MODE`) | `links` | Email body rendering: `plain`, `links`, or `rich` |
| `--quote-replies` (`QUOTE_REPLIES`) | `true` | Quote the parent in outbound replies (email-only, never shown in Matrix) |
| `--bridge-mailboxes` (`BRIDGE_MAILBOXES`) | `false` | Also mirror JMAP mailboxes (Inbox/Sent/…) as their own rooms |
| `--jmap-sync-limit` (`JMAP_SYNC_LIMIT`) | `10` | Max emails fetched per poll |
| `--user` | *(repeatable)* | Declaratively provision a user (see below) |
| `--log-level` (`LOG_LEVEL`) | `info` | `error` \| `warn` \| `info` \| `debug` \| `trace` (global flag) |

If no encryption key is given, credentials are stored in plain text (legacy mode).

### Declarative provisioning

Instead of (or in addition to) interactive login, users can be provisioned at
startup with one repeatable `--user` flag per user — a comma-separated list of
`key=value` pairs:

```bash
--user "mxid=@you:example.com,username=you@mail.example.com,token-file=/run/secrets/jmap"
```

Keys: `mxid` and `username` (required); `url` (JMAP session URL, defaults to
`--jmap-url`); `token-file` (preferred — path to the JMAP token, never exposed in
argv) or `token` (inline); and `matrix-password-file` (optional — enables
double-puppet auto-accept).

> **Deprecated:** the single-user `--jmap-username` / `--jmap-token` flags
> (`JMAP_USERNAME` / `JMAP_TOKEN`) are legacy. Use interactive `!login` or `--user`.

## User guide

Open a Direct Message with the bot (`@_jmap_bot:your.server`) and use these
commands. Messages containing credentials are auto-redacted from the room.

| Command | What it does |
| --- | --- |
| `login` | Start the interactive login (prompts for email, token, then JMAP session URL) |
| `!login <username> <token> <session-url>` | One-shot login |
| `login-matrix <access-token>` | Enable double-puppeting (Element: *Settings → Help & About → Access Token*) |
| `!compose <address> [subject]` | Open a new conversation room with an address you've never mailed (alias: `!email-to`) |
| `!email <to> <subject> <body>` | Send a one-off email |
| `signature <text>` / `signature clear` | Set or clear the signature appended to outbound mail |
| `help` | List commands |

**Replying:** to reply to a bridged email, just type into its room — your message
is sent as an email in that thread. A signature is appended if you've set one.

## Development

The dev shell (`nix develop` / direnv) provides the full toolchain. See
[`AGENTS.md`](AGENTS.md) for the conventions and the inner/outer loops.

```bash
just check     # cargo check
just nextest   # run the test suite (cargo-nextest)
just lint      # clippy + rustfmt --check
nix flake check  # the authoritative build + VM round-trip check
```

## See also

- [`CONTEXT.md`](CONTEXT.md) — domain glossary
- [`docs/adr/`](docs/adr/) — architecture decision records
- [`AGENTS.md`](AGENTS.md) — agent/contributor conventions
- [`nix/module/README.md`](nix/module/README.md) — NixOS module reference
