# Double-puppeting via a one-time login token, not declarative `as_token`

The bridge double-puppets the user's own Matrix account so their own messages appear to be sent by them (not a ghost) and rooms auto-join. This is established with a one-time `login-matrix <access_token>` command (validated via `/whoami`, handled in `src/commands/login_matrix.rs`), with the token persisted in the bridge DB — deliberately **not** the declarative `as_token` method.

The `as_token` double-puppet method requires the shared user to sit in the appservice's user namespace. When that same Matrix user is shared across *more than one* appservice in a deployment (here, this bridge plus a separate mautrix-whatsapp bridge), the homeserver then treats each bridge as "interested" in every room the user is in — including the other bridge's rooms — and floods it with foreign `@_jmap_*` events, `cannot join a room that is not public` spam, and a fatal crypto-token error (confirmed, then reverted). The login-token method causes no namespace pollution.

## Consequences

- Double-puppet setup is a one-time manual step, not declarative; this is a known, accepted trade-off for keeping a single Matrix user shared cleanly across multiple appservices.
- The bridge additionally runs its own scoped `/sync` auto-accept loop (only invites from `@_jmap_bot`) because some homeservers have no homeserver-side auto-accept for invites — notably tuwunel, where `auto_join_rooms` is registration-only.
