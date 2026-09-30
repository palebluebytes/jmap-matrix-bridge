# Backfill processes emails oldest-first to control Matrix room ordering

Historical backfill queries JMAP `received_at` **ascending** (oldest email first) and throttles between batches (`src/sync/backfill.rs`). This is deliberate and counter-intuitive: Element orders its room list by each room's sliding-sync `bump_stamp` — the server stream position of the room's last message — **not** by the message's `origin_server_ts`. Bridging oldest-first means the newest email is sent last and lands at the highest stream position, so the room list sorts newest-first like a mail client.

## Considered Options

- **Newest-first / descending (rejected)** — the obvious choice, but it inverts the room list: the oldest conversation ends up bumped to the top.
- **Oldest-first / ascending (chosen)** — produces the correct newest-first room ordering in Element.

## Consequences

- A future reader who "tidies" the sort to descending will silently break the room-list ordering — the throttle and ascending sort must stay.
- Backfill is detached from live inbound sync and runs at low priority (gated on initial sync completing, with a persisted `backfill_position`) so large accounts don't thrash the homeserver.

## Amendment (2026-09-29): the window, and why its cutoff is persisted

Backfill is optionally bounded by `--backfill-window` / `BACKFILL_WINDOW`
(`1w`, `30d`, `6mo`, …), which adds a JMAP `after` filter — a `receivedAt` floor,
matching the sort key above rather than `sentAfter`, which reads the `Date:`
header and would disagree with the order we page by. Unset means the historical
default: walk everything.

The option exists because an unbounded walk gives every thread in the mailbox its
own Matrix room. That is fine for an account the bridge grew up with, and
actively bad for a large or freshly-imported one — importing a mail archive into a
bridged mailbox otherwise lands thousands of rooms in Element.

The filter is applied to **both** queries that make up the ascending walk — the
bootstrap page in `sync::email::sync_emails` and the `backfill_batch` pages — for
the same reason the position is meaningful at all: they range over one result set.
Filtering only the backfill query bridges the account's oldest mail (what the
window exists to exclude) and then starts backfill at index `sync_limit` of the
narrower windowed set, skipping that many of the oldest in-window mails.

**The cutoff is computed once when a walk starts and persisted** (`backfill_cutoff`
in `jmap_state`, cleared with `backfill_position` on completion). This is not a
caching optimisation, and recomputing it per batch would be a correctness bug:
`position` indexes into the *filtered* result set, so a cutoff that crept forward
between batches would shrink that set from the front and leave the saved position
pointing past mail it had never reached — silently skipping it. Restarts are
routine rather than rare (every deploy restarts the unit, and a large mailbox
takes hours at the default `--jmap-sync-limit` of 10 with its 5s throttle), so an
in-memory anchor would not have survived long enough to be correct.

### Consequences

- Changing the window mid-walk does nothing until that walk completes. Re-anchor
  by stopping the bridge and deleting the user's `backfill_cutoff` row.
- The window never limits **live** sync; new mail is always bridged.
- A window is a date floor only — not a mailbox or keyword filter. Nothing in
  backfill consults `$seen`, so marking mail read does not exempt it.
