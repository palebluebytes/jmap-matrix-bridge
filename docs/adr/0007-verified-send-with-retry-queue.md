# Outbound send is verified, then retried from a durable queue

A Matrix→email send is only treated as delivered once the JMAP `EmailSubmission/set` is confirmed created — `submit()` (`src/sender.rs`) explicitly checks the submission response and errors with "email saved to Sent but the JMAP submission was rejected" otherwise. This closes a real regression where `Email/set` succeeding (the copy lands in Sent) was mistaken for delivery while the submission silently failed, dropping the reply. Failed sends are persisted to an outbound queue and retried by a worker (`src/retry.rs`) on an exponential backoff encoded in the dequeue query (`src/store/queue.rs`): 1, 2, 4, … 256 minutes, capped at 10 attempts, after which the user is notified of permanent failure and the message is purged.

## Considered Options

- **Assume success if `Email/set` succeeds (rejected)** — the original bug; filing the Sent copy is not proof of delivery.
- **Fail immediately, no retry (rejected)** — loses replies to transient JMAP/network failures.
- **Verify submission + durable retry queue with bounded backoff (chosen)** — survives restarts, rides out transient failures, and surfaces give-up to the user rather than dropping silently.

## Consequences

- Backoff arithmetic lives in SQL (the `get_pending_outbound` query computes the next-eligible time), so the database is the single source of truth for retry timing.
- "Give up" is an explicit, user-visible event after ~10 attempts, never a silent drop — the property the VM round-trip check asserts.
