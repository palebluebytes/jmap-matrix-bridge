# JMAP↔Matrix Bridge

A Matrix Application Service that bridges a JMAP email account to Matrix: each email conversation appears as a Matrix room, and replies sent in Matrix go back out as email. This glossary fixes the language used across the code, ADRs, and tests.

## Language

### Identities

**Ghost**:
A Matrix user in the bridge's exclusive `@_jmap_*` namespace that stands in for an email correspondent, so their messages appear in Matrix as a distinct user. Derived one-per-email-address.
_Avoid_: puppet, fake user, virtual user, contact user

**Puppet**:
The bridge acting *as the real user's own* Matrix account (via a stored login token) so that mail the user sent appears authored by them, not by a Ghost. The act of establishing this is **double-puppeting**.
_Avoid_: impersonation, ghost (a Ghost is a correspondent, never the user)

**Bot**:
The single control user the appservice owns (`@_jmap_bot`, the registration's `sender_localpart`). It receives commands, issues invites, and posts bridge notices. Exactly one exists; it is not a Ghost.
_Avoid_: admin, service user, assistant

### Conversation structure

**Thread**:
A JMAP email thread — the RFC 5322 `Message-ID` reference chain that groups related mail. The thread is the unit a Room maps to, one-to-one.
_Avoid_: conversation, chain, discussion

**Room**:
A Matrix room scoped to exactly one Thread. A correspondent's reply within the thread returns to the same Room; a new thread gets a new Room.
_Avoid_: channel, chat, conversation

**Space**:
The Matrix space that groups one user's Rooms, named for that user's own email address.
_Avoid_: folder, group, category

### Mail flow

**Submission**:
The JMAP `EmailSubmission` step that actually hands a message off for delivery. It is distinct from filing the message into Sent (`Email/set`) — a message can be in Sent yet have a rejected Submission, so delivery is only confirmed when the Submission is accepted.
_Avoid_: send, delivery, dispatch (when you specifically mean the EmailSubmission step)

**Backfill**:
The import of a mailbox's pre-existing historical email into Matrix Rooms, run oldest-first and separately from live sync.
_Avoid_: sync, history import, catch-up
