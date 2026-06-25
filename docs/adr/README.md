# Architecture Decision Records

Decisions of record for the JMAP↔Matrix bridge. See [`CONTEXT.md`](../../CONTEXT.md)
for the domain vocabulary these are written in.

| ADR | Decision |
| --- | --- |
| [0001](0001-custom-jmap-matrix-bridge.md) | A hand-written bridge with one Matrix room per email thread |
| [0002](0002-double-puppet-via-login-token.md) | Double-puppeting via a one-time login token, not declarative `as_token` |
| [0003](0003-tokio-async-locks-over-dashmap.md) | Shared state uses `tokio` async `RwLock`/`Mutex`, not DashMap |
| [0004](0004-thread-via-rfc5322-message-ids.md) | Outbound replies thread via RFC 5322 `Message-ID`s, not JMAP ids |
| [0005](0005-backfill-oldest-first.md) | Backfill processes emails oldest-first to control Matrix room ordering |
| [0006](0006-single-instance-db-coordination.md) | Single-instance design; coordination lives in the database |
| [0007](0007-verified-send-with-retry-queue.md) | Outbound send is verified, then retried from a durable queue |
| [0008](0008-ci-and-release-flow.md) | CI is `nix flake check`; releases are on-demand via release-plz |
| [0009](0009-one-jmap-account-per-matrix-user.md) | One JMAP account per Matrix user; multi-account migration path recorded |
| [0010](0010-permission-model.md) | Access is a default-deny permission map with `user`/`admin` levels |
| [0011](0011-command-emoji-duality.md) | Every action is a text command; emoji reactions are optional shortcuts |
| [0012](0012-matrix-actions-replicate-as-reversible-moves.md) | Matrix-side deletes replicate to the mailbox as Trash/Junk moves, never destroys |
| [0013](0013-end-to-bridge-encryption-deferred.md) | End-to-bridge encryption is a recorded goal, deferred and off-by-default |
| [0014](0014-automatic-double-puppet-via-shared-secret.md) | Optional automatic double-puppet for local interactive users via shared-secret-auth |
| [0015](0015-read-state-jmap-to-matrix-requires-double-puppet.md) | Read-state syncs JMAP→Matrix via puppet receipts; needs double-puppet |
