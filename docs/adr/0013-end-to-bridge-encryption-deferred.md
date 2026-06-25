# End-to-bridge encryption is a recorded goal, deferred and off-by-default

Optional end-to-bridge encryption (Megolm rooms, so the homeserver never stores
plaintext) is a legitimate goal but is **deferred**: not built yet, and **off by
default** when it is. This records the decision, the threat model that bounds its
value, and the implementation path, so it is a costing exercise later rather than a
re-litigation.

Current state: `matrix-sdk` is compiled with `default-features = false` (only
`rustls-tls`) — no `e2e-encryption` — and the bridge sends as ghosts via **raw
appservice-token masquerading** (`send_as_ghost` in `src/matrix.rs`), not via
`matrix-sdk` `Client` sessions. So encryption is a substantial build, not a flag.

## Threat model (what bounds the value)

The bridge **always sees plaintext** — it converts cleartext email to and from
Matrix — and the **JMAP/mail provider holds all the mail** regardless. Megolm on
the Matrix rooms therefore protects content from exactly one party: **a homeserver
the user does not fully trust** (its admins, database, and backups). It does *not*
protect against the bridge operator, the mail provider, or SMTP in transit.

Consequently the value is deployment-dependent:

- **Self-hosting homeserver + bridge together** (the documented NixOS path): the
  user already trusts that machine — marginal value is low.
- **Bridge against a third-party homeserver**: real value — the homeserver never
  sees plaintext.

This split is why building now isn't justified: the one missing "core bridge
feature" that's both large *and* whose payoff a large fraction of deployments won't
use.

## Considered Options

- **Build now (rejected)** — weeks of appservice-crypto work for a benefit many
  self-hosting deployments don't need.
- **Never support it (rejected)** — strands exactly the users with the strongest
  case (those on a homeserver they don't run).
- **Record the design, defer the build, off-by-default when it lands (chosen)** —
  mirrors the multi-account treatment in
  [ADR-0009](0009-one-jmap-account-per-matrix-user.md): commit the road, pour the
  asphalt on demand.

## Implementation path (when built)

- **Enable `matrix-sdk`'s `e2e-encryption` feature** and add a crypto store
  (its SQLite crypto store, kept separate from the app DB or as a second pool).
  This is independent of `src/crypto.rs`, which is AES-at-rest for credentials and
  unrelated to Megolm.
- **The hard part is appservice masquerading + E2EE.** Each ghost and the bot is a
  distinct Matrix identity that must hold Olm device keys, publish one-time keys,
  and receive to-device messages and device-list updates. The viable path is
  **per-identity `OlmMachine`s** fed by **MSC3202** (encrypted appservices: the
  homeserver delivers to-device events and device-list changes inside the
  appservice transaction). This is **gated on homeserver support** — verify the
  target homeserver (e.g. the tuwunel/conduit family this project documents)
  implements MSC3202; **fall back to plaintext rooms when it doesn't**, never fail
  to bridge.
- **Rejected sub-approaches:** encrypting only the bot's own messages (ghosts post
  most content, so this leaks nearly everything); spinning a full `matrix-sdk`
  `Client` login session per ghost (session-management weight the raw-masquerade
  design exists to avoid).
- **Config:** `encryption.allow` / `encryption.default` flags, both off by default;
  a key-sharing policy for users already in the room.
- **Double-puppet interplay:** the User's own Matrix devices
  ([ADR-0002](0002-double-puppet-via-login-token.md)) must receive room keys, or the
  User can't read their own bridged mail — key sharing must target them.
- **Backfill interplay:** historically imported mail
  ([ADR-0005](0005-backfill-oldest-first.md)) must be encrypted on the way in too;
  confirm the backfill path encrypts rather than bypassing the Megolm session.
