# Shared state uses tokio async `RwLock`/`Mutex`, not DashMap

Shared mutable state — the per-user JMAP client map and poller handles (`src/client_manager.rs`), in-flight login flows (`src/state.rs`), and the puppet dedup set (`src/puppet.rs`) — is guarded by `tokio::sync::{RwLock, Mutex}` wrapping a plain `HashMap`/`HashSet`, **not** `dashmap::DashMap`. `dashmap` is not a dependency.

These maps hold one entry per bridged user and are touched only on cold events (login, logout, poller start/stop, a send). Each guard is held just long enough to clone an `Arc<Client>` out or insert/remove a handle, then dropped — there is no hot path where lock-free sharding would matter. The locks are acquired with `.await` because the surrounding code is pervasively async; DashMap's guards are synchronous and `!Send` across an `.await`, which is an easy deadlock/`Send`-bound footgun in this codebase and earns nothing without contention to relieve.

## Considered Options

- **`dashmap::DashMap` (rejected)** — sharded lock-free maps shine under high-frequency concurrent access; this bridge has none. Its sync guards also fight the async call sites.
- **`tokio` async `RwLock`/`Mutex` over a `HashMap` (chosen)** — async-aware, briefly held, trivial to reason about at this scale.

## Consequences

- This deliberately overrides the earlier "use DashMap, avoid `RwLock`/`Mutex`" guidance, which predated the per-user async design; AGENTS.md §3 has been corrected to match.
- If a genuinely hot, high-contention shared map ever appears, revisit — DashMap or a sharded design may then earn its place.
