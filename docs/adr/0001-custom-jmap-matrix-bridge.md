# A hand-written JMAP↔Matrix email bridge, with per-thread rooms

Email-to-Matrix bridging is done by a bespoke Rust appservice (the crate in `src/`, packaged and wired as a NixOS service via `nix/module/`) that talks JMAP to Stalwart and the appservice API to a Matrix homeserver. We built our own rather than adopt an off-the-shelf email bridge so the behaviour — threading model, body rendering, double-puppeting (see [0002](0002-double-puppet-via-login-token.md)), retry/submission semantics — is fully under our control and testable in a VM round-trip check (`nix/check/`).

The central domain decision is that a bridged conversation's Matrix room is scoped **per email thread**, not per correspondent: an outbound reply shares the inbound JMAP thread and references its real `Message-ID`, and a contact's reply lands back in the same room. Rooms are grouped under a single private space named for the user's own address.

## Considered Options

- **One room per thread (chosen)** — a deliberate boundary choice.
- **One room per contact** — rejected; it collapses distinct conversations with the same correspondent into one timeline.

## Consequences

- Owning the bridge means owning the bug surface: the bridge carries hard-won regression tests for issues that only a real populated mailbox exposed (self-ingestion of the Sent copy, the appservice echo loop, CASCADE-wiping room bindings on restart). Don't "simplify" those guards away.
