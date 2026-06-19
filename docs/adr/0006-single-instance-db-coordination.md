# The bridge is single-instance; coordination lives in the database

The bridge is designed to run as exactly one process. Per-user runtime state (JMAP clients, poller handles, in-flight logins) is held in-memory (see [ADR-0003](0003-tokio-async-locks-over-dashmap.md)), and the one piece of cross-task coordination that needs durability — serializing room creation so two concurrent emails in a new thread don't spawn duplicate rooms — is done with a `room_creation_locks` table whose `UNIQUE` constraint is the lock (`src/store/sync.rs`). Stale locks are cleared on startup (`src/store/connection.rs`), which is sound precisely because only one instance ever runs.

## Considered Options

- **In-process `Mutex` for room-creation serialization (rejected)** — wouldn't survive a restart mid-creation, and conflates lock lifetime with task lifetime.
- **External distributed lock (Redis/etcd) (rejected)** — needless infrastructure for a single-tenant-per-process email bridge.
- **DB-row lock + clear-on-startup (chosen)** — durable within a run, self-healing across restarts, no extra moving parts.

## Consequences

- **No horizontal scaling.** Running two instances against the same database would double-process JMAP pushes and let both reclaim each other's "stale" locks on startup. This constraint is invisible in the code until someone tries to run a replica — hence this record.
- Scaling, if ever needed, is per-user sharding across processes (disjoint user sets), not two instances sharing one DB.
