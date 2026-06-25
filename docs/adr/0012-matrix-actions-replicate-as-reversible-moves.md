# Matrix-side actions replicate to the mailbox as reversible moves, never destroys

Acting on a Room is acting on its Thread: the bridge replicates destructive
Matrix-side actions back to the JMAP mailbox, but **only as reversible mailbox
moves** (to the `role=trash` or `role=junk` mailbox via `Email/set` on
`mailboxIds`) — **never** as a permanent `Email/set destroy`. Trash is the safety
net; the mail server's own retention does the eventual purging.

## Hide vs. trash

A plain room **leave** is non-destructive — "stop showing me this," not "throw it
away." It never moves mail. Trashing requires an **explicit** trigger, per the
command/reaction duality ([ADR-0011](0011-command-emoji-duality.md)):

| Action | Trigger | Mailbox effect |
| --- | --- | --- |
| Trash a thread | `delete-room` / 🗑️ | move the **whole Thread** to `role=trash` |
| Junk a thread | `spam` / 🚫 | move the **whole Thread** to `role=junk` |
| Leave / hide | room leave | none |

**Granularity is thread-level.** A 🗑️/🚫 reaction on any single message trashes or
junks the *entire* Thread, not just the reacted message — the Room maps one-to-one
to the Thread, and tearing the Room down while leaving some of its mail live is
incoherent. Per-message trash is a deliberate non-goal (see Consequences).

The JMAP move runs through the requesting User's own JMAP client (their
credentials), so it is authentically their action — no impersonation question.

## logout

`logout` always logs out, immediately — a User logging out is not waiting on mail.
It stops the JMAP client + event loop, clears stored JMAP credentials, and
**abandons** that User's pending `outbound_queue` entries (unsent mail is dropped,
not flushed). Rooms, ghosts, the space, and mappings are kept so a later `login`
resumes in place. The Matrix double-puppet token is a separate lifecycle and is
left untouched.

## Edits, redactions, and the send-delay window

Native Matrix edits and redactions are *also* Matrix-side actions, but unlike trash
they can only honestly act on mail the server has not yet committed — email has no
recall and no edit-after-send. To make that window reliably useful rather than
incidental, outbound mail is **held briefly before the JMAP submission**
([ADR-0007](0007-verified-send-with-retry-queue.md)) — a Gmail-style undo window.

- **Send-delay.** Every outbound message sits in `outbound_queue` with a
  "release at" time before it is submitted. Default **5 seconds**; a per-User
  setting via `send-delay <seconds>` / `send-delay off` (stored like `signature`,
  [ADR-0011](0011-command-emoji-duality.md) text-only command), capped at **300s**.
  An operator-set global default can move the 5s baseline.
- **Redact a held message** → cancel it (pulled from the queue before release); the
  email is never submitted. Redacting a message that is *retrying* after a failed
  submission likewise cancels the retry.
- **Edit a held message** → rewrite its queued body before release.
- **Redact / edit after submission** → no-op plus a one-line notice; the mail is
  already gone.
- **Redact an inbound (ghost) message** → local Matrix-only, no mailbox effect.
  Trashing mail is 🗑️'s job, not redaction's.

**Send-state is shown, never silent.** A successful send previously produced no
confirmation at all (only failures spoke). The Bot now places a state reaction on
the outbound message — ⏳ **held** (window open, redact to undo) → ✅ **submitted**
(verified) → ❌ **failed** (alongside the existing give-up notice) — and posts a
one-time per-room hint explaining the hold and the glyphs. The ⏳→✅ transition *is*
the window closing, so "what's happening" is glanceable rather than mysterious.

## Considered Options

- **Never touch the mailbox (rejected)** — treat the Room as a read-only view;
  delete-room just discards the view. Coherent, but it makes the bridge a
  second-class mail client: you can read and reply but never tidy your mailbox from
  Matrix. The Room *is* the Thread, so actions on it should land server-side.
- **Replicate including hard destroy (rejected)** — mirror a delete as
  `Email/set destroy`. A fat-fingered reaction or stray redaction would then
  permanently, irrecoverably delete mail. Unacceptable for the safety it removes.
- **Reversible moves only (chosen)** — full replication of intent with Trash/Junk as
  an undo path, matching what "delete" means in every mainstream mail client.
- **Redaction/edit as no-op-with-notice in all cases (rejected)** — simplest, but
  wastes the durable queue we already have and offers no unsend. The hold window
  turns that queue into a real undo affordance for near-zero extra machinery.

## Consequences

- **No permanent ignore-list.** A trashed Thread that receives new mail simply gets
  a fresh Room — you trashed the old conversation, not muted the correspondent.
- **`spam` is the one "don't show me this again" affordance** the design offers, and
  it works by moving to Junk (where the server may also learn), not by a local mute.
- **If the account has no `role=trash` / `role=junk` mailbox**, the move can't be
  performed: the bridge unbridges the Room locally and posts a notice that it could
  not move the mail server-side, rather than failing silently or guessing a mailbox.
- **Per-message trash is out of scope.** If it's ever wanted, it can't reuse the
  Room-teardown path (the Room must survive); it would be a distinct message-level
  action, recorded then.
- A plain **leave** stopping at "non-destructive" means the exact re-bridging
  behavior when new mail later arrives in a left Thread is an implementation detail,
  not a mailbox mutation — it never surprises the server.
- **Mail is not sent instantly** — the 5s default hold is deliberate (the undo
  window) and made visible by the ⏳ state reaction and the one-time hint, not
  hidden. An operator who wants instant send sets the global default to 0.
