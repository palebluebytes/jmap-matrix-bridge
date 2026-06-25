# One JMAP account per Matrix user

A bridged **User** is exactly one Matrix account paired one-to-one with exactly
one JMAP account. The bridge does **not** support a single Matrix user logging in
multiple mailboxes (the mautrix "multiple logins per user, each with a login ID"
model). Someone who needs two mailboxes uses two Matrix accounts.

This is already a hard invariant in the schema: `users.matrix_user_id` is the
PRIMARY KEY, and every per-user table — `jmap_state`, `user_signatures`,
`outbound_queue`, `room_ghost_mapping` — foreign-keys to it (`migrations/20260516000000_initial.sql`).
`ClientManager` keys its one JMAP client and one event loop per `matrix_user_id`.
Recording the boundary so command design (`logout`, `ping`/`status`, the
permission model) can assume a single account without carrying a login selector,
and so a future reader sees the single-account assumption is deliberate, not an
oversight.

## Considered Options

- **Multiple logins per user, login-ID-keyed (rejected)** — mautrix's model. Taxes
  every command with a `<login ID>` selector forever and requires a per-room
  "preferred login" concept, to serve a use-case email users rarely have (a mailbox
  is not a phone number — you can have many, cheaply, on separate Matrix accounts).
- **Single account per Matrix user (chosen)** — matches the existing schema, keeps
  every command argument-free, and the space-per-user model makes the
  "second Matrix account per mailbox" escape hatch pleasant rather than a kludge.

## Consequences

- **`login` while already logged in** replaces the existing account (or is refused
  with "already logged in as X — `logout` first"); it never adds a second.
- **`logout`, `ping`/`status`** take no account argument — they act on the one
  account bound to the requesting Matrix user.
- **No "preferred login" / per-room account selection** is needed: a room is bound
  to its owning user via `room_ghost_mapping.matrix_user_id`, and that user has
  exactly one account, so the send identity is unambiguous.
- **The painful case if we ever reverse this** is "personal + work mailbox in one
  Matrix account." The migration path below exists so that demand can be costed,
  not so it is pre-built.

## Future migration path (if multi-account is ever required)

Recorded so this is a costing exercise, not an archaeology one. Do **not** build
any of this speculatively — it is the escape hatch, not a roadmap.

**Introduce an account surrogate key.** Add `accounts.account_id` (surrogate PK)
with a `UNIQUE (matrix_user_id, jmap_username)` constraint; `matrix_user_id`
becomes a non-unique column. Backfill one `account_id` per existing `users` row in
the same migration so existing installs convert transparently.

**Re-point per-account FKs from `matrix_user_id` to `account_id`:**

- `jmap_state` — JMAP sync state is per mailbox → key on `account_id`.
- `outbound_queue` — the queue worker must know which JMAP client sends → add
  `account_id`; `idx_outbound_queue_user` becomes `idx_outbound_queue_account`.
- `room_ghost_mapping` — a thread room belongs to one mailbox → add `account_id`.
  This is the linchpin: it makes the room→account binding explicit, which is what
  lets reply-in-room stay argument-free under multi-account.
- `user_signatures` — decide whether a signature is per-user or per-mailbox
  (likely per-mailbox → `account_id`).
- `jmap_thread/message/mailbox_mapping` are keyed on opaque JMAP ids, which are only
  unique *within* an account. Under multi-account they need an `account_id` column
  added to their PKs to avoid cross-account id collisions.

**`ClientManager`** changes from `matrix_user_id → (client, event loop)` to
`account_id → (client, event loop)`, plus a `matrix_user_id → {account_id}` index
for command routing. One event loop per account, unchanged otherwise.

**Commands grow a selector** only where the target is ambiguous: `login` always
adds (never replaces); `logout <account>` and `ping` list/act per account;
`!compose`/`!email` need a "send as" — reuse mautrix's `set-preferred-login`
(a per-Matrix-user default account) rather than an argument on every send. Reply in
an existing room stays argument-free because the room already names its account.

**Spaces** already key one space per user named for the email address
(`src/ghost.rs`); under multi-account this becomes naturally one space per account,
no structural change.

**Double-puppeting stays per Matrix user, not per account** — the puppet is the
human's own Matrix identity (ADR-0002), shared across all their mailboxes. The
`puppet_tokens` storage stays keyed on `matrix_user_id`.

The schema migration touching every per-user table, plus the per-room account
binding, is the bulk of the cost; the command and `ClientManager` changes are
mechanical once the keys exist.
