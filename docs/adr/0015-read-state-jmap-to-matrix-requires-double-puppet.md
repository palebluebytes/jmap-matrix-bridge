# Read-state syncs JMAP→Matrix via puppet receipts, and only with double-puppet

Read state is bridged both ways. Matrix→JMAP already exists: reading in a Room adds
the `$seen` keyword (`mark_as_read`, `src/sender.rs`). This adds the reverse: when
an email gains `$seen` elsewhere (read in another mail client), the bridge emits an
`m.read` receipt on that Thread's message, hooking the existing `Email/changes`
`updated()` path (`src/sync/email.rs`).

**The receipt is sent as the User's own Matrix account (the double-puppet,
[ADR-0002](0002-double-puppet-via-login-token.md)).** A read receipt must originate
from the real user; the appservice cannot set read-state for an arbitrary user.
Therefore **read-state sync JMAP→Matrix works only when double-puppet is enabled**;
without it the bridge silently no-ops (there is nothing it can do), and `status`
reports double-puppet state so the user can tell why. This dependency is the reason
to record the decision — it is otherwise a surprising silent gap.

## Considered Options

- **Bot/ghost-sent read marker (rejected)** — a receipt from `@_jmap_bot` or a ghost
  marks *their* read-state, not the user's; it does nothing useful for the user's
  own unread count.
- **Skip JMAP→Matrix read-state (rejected)** — leaves Matrix showing mail unread
  that the user already read elsewhere; the asymmetry is exactly the gap.
- **Puppet `m.read` receipt on `$seen`, double-puppet-gated (chosen)** — correct
  semantics, reuses the existing change-sync path, degrades cleanly without
  double-puppet.

## Consequences

- **Loop-gated.** Matrix-read → `$seen` → `Email/changes` reports the email updated,
  which would re-emit a receipt. The emit is gated on an unseen→seen transition (and
  is idempotent anyway), consistent with the existing "don't re-bridge our own mail"
  guards in `src/sync/email.rs`.
- **Mark-unread (removing `$seen`) is out of scope for now.** Matrix has no settled
  unread-marking standard (`m.marked_unread` / MSC2867 has patchy client support)
  and the value is marginal. Read→read is the dominant case; mark-unread can be
  added later if demand appears.
- Read-state fidelity becomes one more reason to enable double-puppet, alongside
  authored-by-you messages and auto-join.
