# Every user action is a text command, with an optional emoji-reaction shortcut

The canonical, always-present way to invoke any user action is an explicit **text
command**. A message- or room-scoped action *may* additionally offer an **emoji
reaction** as a shortcut, but the reaction is never the only way to trigger it.

This makes the text command the single source of truth: `help` can enumerate every
action, the suite can test it without synthesizing reaction events, and clients
that render reactions poorly (or hide them) never strand a user with no way to act.

This formalizes a convention the codebase was already half-following and pins down
the one place it wasn't: **load-images was reaction-only** (the 🖼️ annotation in
`src/services/images.rs`), with no text equivalent. It gains a `show-images`
command; the 🖼️ reaction stays as its shortcut.

## Action ↔ command ↔ reaction

Emoji glyphs are tunable; the binding (every action has a command, reactions are
optional sugar) is the decision.

| Action | Text command (canonical) | Emoji shortcut |
| --- | --- | --- |
| Load remote images for a message | `show-images` | 🖼️ |
| Move a thread to Trash | `delete-room` | 🗑️ |
| Move a thread to the Junk mailbox | `spam` | 🚫 |

Pure setup/control actions that are neither message- nor room-scoped (`login`,
`logout`, `ping`, `version`, `signature`, …) are text-only — there is nothing for a
reaction to attach to. "Optional" means exactly that: an action without a sensible
target gesture simply has no emoji.

## Consequences

- **`help` lists every action by its text command**, and notes the emoji shortcut
  where one exists.
- **New actions add the text command first.** A reaction-only action is a defect,
  not a shortcut.
- Reaction handlers stay thin: they resolve their target event/room and then call
  the same code path the text command does, so behavior can't drift between the two.
