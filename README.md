# JMAP Matrix Bridge

[![CI](https://github.com/palebluebytes/jmap-matrix-bridge/actions/workflows/ci.yml/badge.svg)](https://github.com/palebluebytes/jmap-matrix-bridge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/palebluebytes/jmap-matrix-bridge?sort=semver)](https://github.com/palebluebytes/jmap-matrix-bridge/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A Rust [Matrix Application Service](https://spec.matrix.org/latest/application-service-api/)
that bridges a [JMAP](https://jmap.io/) email account (Stalwart, Fastmail, …) into
Matrix: each email conversation becomes a Matrix room, and messages you send in
Matrix go back out as email.

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
  Matrix account rather than a ghost. It can be automatic, via a shared secret with
  the homeserver, or a token the user pastes once.
  ([ADR-0014](docs/adr/0014-automatic-double-puppet-via-shared-secret.md))
- **Push-driven sync.** The bridge subscribes to JMAP EventSource pushes, debounces
  them, and reconciles missed pushes via JMAP state tokens (with an hourly heartbeat
  poll as a backstop) — it does not poll on a fixed interval. Historical mail is
  imported separately, oldest-first, so Element's room list sorts newest-first.
  ([ADR-0005](docs/adr/0005-backfill-oldest-first.md))
- **Read state syncs both ways.** Reading a room marks the mail read, and marking
  mail read elsewhere marks the room read — the latter needs double-puppeting.
  ([ADR-0015](docs/adr/0015-read-state-jmap-to-matrix-requires-double-puppet.md))
- **Attachments cross in both directions.** Inbound attachments are uploaded into
  the room; files you send in Matrix go out as email attachments.
- **Verified outbound delivery.** A Matrix→email send is only treated as delivered
  once the JMAP `EmailSubmission` is confirmed; failures go to a durable retry queue
  with exponential backoff and a user-visible give-up notice.
  ([ADR-0007](docs/adr/0007-verified-send-with-retry-queue.md))
- **Default-deny access.** Only senders you grant may use the bridge; by default
  that is your own homeserver's users and nobody else.
  ([ADR-0010](docs/adr/0010-permission-model.md))
- **Trash and junk replicate as reversible moves.** Acting on a room acts on the
  thread — never as a permanent destroy.
  ([ADR-0012](docs/adr/0012-matrix-actions-replicate-as-reversible-moves.md))

The vocabulary used throughout (ghost, puppet, bot, thread, room, space,
submission, backfill) is defined in [`CONTEXT.md`](CONTEXT.md); the decisions
behind it are recorded in [`docs/adr/`](docs/adr/README.md).

## Limitations

Worth knowing before you deploy:

- **No Matrix end-to-end encryption.** The bridge does not implement E2EE, so
  bridged rooms are unencrypted, and media sent *from* an encrypted room cannot be
  forwarded. Recorded and deferred, pending MSC3202
  ([ADR-0013](docs/adr/0013-end-to-bridge-encryption-deferred.md),
  [#29](https://github.com/palebluebytes/jmap-matrix-bridge/issues/29)).
- **One JMAP account per Matrix user.** Two mailboxes means two Matrix accounts
  ([ADR-0009](docs/adr/0009-one-jmap-account-per-matrix-user.md),
  [#30](https://github.com/palebluebytes/jmap-matrix-bridge/issues/30)).
- **Single instance.** One process; there is no HA or scale-out story. Cross-task
  coordination lives in the database
  ([ADR-0006](docs/adr/0006-single-instance-db-coordination.md)).
- **Linux only.** No native macOS or Windows build — on those it runs inside a
  Linux container like any other Linux daemon.
- **Trash is per-thread, and there is no mute.** Trashing or junking acts on the
  whole thread, and a trashed thread that receives new mail comes back as a fresh
  room — you trashed a conversation, not muted a correspondent
  ([ADR-0012](docs/adr/0012-matrix-actions-replicate-as-reversible-moves.md)).
- **Read state from mail to Matrix needs double-puppeting.** Without it, marking
  mail read in your mail client silently does nothing in Matrix.

## Try it without setting anything up

```bash
nix run .#playground     # or: just playground
```

This boots a disposable VM containing a real mail server (Stalwart), a real
homeserver (tuwunel), the bridge built from your checkout, and a desktop Matrix
client — already registered, already logged in, with one inbound email waiting as
a bridged room. Reply to it and the mail is genuinely delivered inside the VM.

Requires `/dev/kvm`, `x86_64-linux` and a graphical session. It is a sandbox, not
a deployment — see [`nix/playground/README.md`](nix/playground/README.md).

## Install

Full options — NixOS module, Nix package, container image, static binary, from
source — with the architecture matrix and provenance verification, are in
[`docs/install.md`](docs/install.md). The two most common:

```bash
# NixOS: the flake exposes overlays.default and nixosModules.jmap-bridge
#        (see nix/module/README.md) — the recommended production deploy.

# Anything else: the public multi-arch container image
docker pull ghcr.io/palebluebytes/jmap-matrix-bridge:latest
docker run --rm ghcr.io/palebluebytes/jmap-matrix-bridge:latest run --help
```

`:latest` tracks the newest release; pin `:vX.Y.Z` for production.

## Configure & run

### 1. Generate a registration file

```bash
jmap-matrix-bridge generate-registration \
  --url http://localhost:8008 \
  --output registration.yaml
```

This writes a registration (to `registration.yaml` by default) with id
`jmap-bridge`, sender localpart `_jmap_bot` (the bot user, e.g.
`@_jmap_bot:your.server`), and the `@_jmap_.*` user namespace. Load it into your
homeserver — for tuwunel, drop it into the `appservice_dir`; for Synapse or
Dendrite, reference it from the homeserver config.

> Generate the file rather than hand-writing one. It also requests ephemeral
> events (`receive_ephemeral`), which read-state sync depends on — a hand-written
> registration without it loses that feature silently.

### 2. Run the service

The bridge runs as a daemon — typically via the NixOS module (see
[`nix/module/`](nix/module/) and its [README](nix/module/README.md)). A manual
invocation:

```bash
jmap-matrix-bridge run \
  --jmap-url https://mail.example.com/.well-known/jmap \
  --matrix-url http://localhost:6167 \
  --matrix-as-token-file /run/secrets/jmap-bridge-as-token \
  --matrix-hs-token-file /run/secrets/jmap-bridge-hs-token \
  --matrix-domain example.com \
  --port 8008 \
  --db sqlite:bridge.db \
  --encryption-key-file /run/secrets/jmap-bridge-key
```

Every flag has an environment-variable equivalent, and
`jmap-matrix-bridge run --help` is the authoritative list. See
[`docs/configuration.md`](docs/configuration.md) for the full table plus the
permission model, double-puppeting setup and declarative user provisioning.

If no encryption key is given, credentials are stored in plain text (legacy mode).

### 3. Log in

Open a Direct Message with the bot (`@_jmap_bot:your.server`) and send `login`.
It prompts for your email address, your JMAP token, and the JMAP session URL.
Messages containing credentials are auto-redacted from the room.

Users can also be provisioned declaratively at startup with `--user`, skipping
the interactive step entirely — see
[`docs/configuration.md`](docs/configuration.md#declarative-provisioning).

## User guide

Commands go in the bot DM; the ones marked *in an email room* go in a bridged
room. Where an emoji is listed, reacting with it does the same thing as the
command ([ADR-0011](docs/adr/0011-command-emoji-duality.md)). Every command also
accepts a `!` prefix.

| Command | Emoji | What it does |
| --- | :---: | --- |
| `login` | | Start the interactive login (prompts for email, token, then JMAP session URL) |
| `!login <username> <token> <session-url>` | | One-shot login |
| `login-matrix <access-token>` | | Enable double-puppeting by hand (Element: *Settings → Help & About → Access Token*). Not needed if the operator configured automatic double-puppeting |
| `logout` | | Disconnect your JMAP account, keeping your rooms. Unsent mail is dropped |
| `status` (alias `ping`) | | Show connection and sync status |
| `sync` | | Reconcile mail now and re-file your rooms into the email space |
| `!compose <address> [subject]` | | Open a new conversation room with an address you've never mailed (alias: `!email-to`) |
| `!email <to> <subject> <body>` | | Send a one-off email |
| `signature <text>` / `signature clear` | | Set or clear the signature appended to outbound mail |
| `show-images` *(in an email room)* | 🖼️ | Load an HTML mail's remote images. As a command it must be **sent as a reply** to the message |
| `delete-room` *(in an email room)* | 🗑 | Move the whole thread to Trash and unbridge the room |
| `spam` *(in an email room)* | 🚫 | Move the whole thread to Junk and unbridge the room |
| `help` | | List commands |

**Replying:** to reply to a bridged email, just type into its room — your message
is sent as an email in that thread, with your signature appended if you've set one.

**Attachments:** files you send in a bridged room go out as email attachments. The
email body is a fixed line rather than your caption, so anything you want the
recipient to read should be a separate message.

## Troubleshooting

**Watch the logs.** Under systemd:

```bash
journalctl -u jmap-bridge -f
```

Raise detail with `--log-level debug` (`LOG_LEVEL=debug`). A send logs
`Sending fresh email…` / `Sending ghost room reply…` and then either
`Submitted outbound message N` or the failure plus `adding to retry queue`.

**A message got a ❌ reaction.** Delivery failed permanently — the bridge retried
with backoff, gave up after ten attempts, and posted a notice alongside the
reaction. The logs above say why the JMAP submission was rejected.

**Mail seems stuck.** Inspect the outbound queue directly:

```bash
sqlite3 /var/lib/jmap-bridge/bridge.db \
  'SELECT id,event_id,release_at,retry_count FROM outbound_queue;'
```

Redacting a message that is sitting in retry backoff cancels it.

**Sends fail on a fresh mail server.** The bridge binds every `EmailSubmission` to
whatever `Identity/get` returns, and some servers — Stalwart among them — never
auto-create an identity. If the account has no identity, every send fails. Servers
may also reject addresses on `localhost` or reserved TLDs like `.test` when
creating one, so use a real dotted domain.

**`delete-room` / `spam` report they couldn't move the mail.** The account has no
`role=trash` or `role=junk` mailbox. The bridge unbridges the room locally rather
than guessing a mailbox or failing silently.

**Read state doesn't sync from mail to Matrix.** Either double-puppeting isn't set
up (it is required for this direction), or the homeserver isn't sending ephemeral
events — check the registration has `receive_ephemeral`.

**A user can't do anything, not even `login`.** Access is default-deny. Unless you
passed `--permission`, only users on the bridge's own `--matrix-domain` are
granted; everyone else, including federated senders, is refused. See
[`docs/configuration.md`](docs/configuration.md#permissions).

## Development

The dev shell (`nix develop` / direnv) provides the full toolchain. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) to get started and [`AGENTS.md`](AGENTS.md)
for the full conventions.

```bash
just check       # cargo check
just nextest     # run the test suite (cargo-nextest)
just lint        # clippy + rustfmt --check
just playground  # boot the sandbox VM
nix flake check  # the authoritative build + VM round-trip check
```

## Releases

CI runs a single authoritative gate on every PR — `nix flake check` (build +
clippy + rustfmt + unit tests + the VM round-trip), across x86_64 and aarch64.

Versioning is automated with [release-plz](https://release-plz.dev): every merge to
`main` keeps a "release vX.Y.Z" PR up to date with a generated `CHANGELOG.md`, and
**cutting a release is merging that PR** — which tags the version, publishes the
GitHub Release, and builds the static binaries + container image. The full flow and
rationale are in [ADR-0008](docs/adr/0008-ci-and-release-flow.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise,
any contribution intentionally submitted for inclusion in this crate by you, as
defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.

## See also

- [`docs/install.md`](docs/install.md) — every delivery method, with provenance verification
- [`docs/configuration.md`](docs/configuration.md) — flags, permissions, provisioning
- [`CONTEXT.md`](CONTEXT.md) — domain glossary
- [`docs/adr/`](docs/adr/README.md) — architecture decision records
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to work on the bridge
- [`AGENTS.md`](AGENTS.md) — agent/contributor conventions
- [`nix/module/README.md`](nix/module/README.md) — NixOS module reference
- [`nix/playground/README.md`](nix/playground/README.md) — the sandbox VM
