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
