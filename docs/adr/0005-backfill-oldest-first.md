# Backfill processes emails oldest-first to control Matrix room ordering

Historical backfill queries JMAP `received_at` **ascending** (oldest email first) and throttles between batches (`src/sync/backfill.rs`). This is deliberate and counter-intuitive: Element orders its room list by each room's sliding-sync `bump_stamp` — the server stream position of the room's last message — **not** by the message's `origin_server_ts`. Bridging oldest-first means the newest email is sent last and lands at the highest stream position, so the room list sorts newest-first like a mail client.

## Considered Options

- **Newest-first / descending (rejected)** — the obvious choice, but it inverts the room list: the oldest conversation ends up bumped to the top.
- **Oldest-first / ascending (chosen)** — produces the correct newest-first room ordering in Element.

## Consequences

- A future reader who "tidies" the sort to descending will silently break the room-list ordering — the throttle and ascending sort must stay.
- Backfill is detached from live inbound sync and runs at low priority (gated on initial sync completing, with a persisted `backfill_position`) so large accounts don't thrash the homeserver.
