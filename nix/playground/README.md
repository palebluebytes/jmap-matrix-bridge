# Playground VM

A **disposable local sandbox** that boots the whole bridge stack in one QEMU VM
**with a graphical desktop and a Matrix client already open**, so you can click
through the bridge by hand:

- **Stalwart** — a real JMAP mail server (the bridge's "email" side)
- **tuwunel** — a real Matrix homeserver (the bridge's "chat" side)
- **the bridge** — built from *this* checkout, wired to both
- **XFCE + nheko** — a lightweight desktop that auto-opens a Matrix client

Watch an inbound email turn into a Matrix room, reply into it and have the mail
really delivered, and drive the 🖼️/🗑/🚫 reactions. This is the interactive
counterpart to the headless round-trip test in [`nix/check`](../check); it reuses
the same service wiring.

> ⚠️ **Not a deployment.** Plaintext credentials, auto-login, throwaway secrets.
> It's a disposable local box — never expose it.

Requires `/dev/kvm` and `x86_64-linux` (like the VM check), and a graphical host
session for QEMU to open its window in.

---

## Boot it

```bash
nix run .#playground          # or: just playground
```

A **QEMU window opens** showing an XFCE desktop (it auto-logs-in the `tester`
user). First boot builds/pulls a big closure (desktop + client), so it takes a
while; later boots are fast. Leave the launching terminal running — it hosts the
VM. Shut down from the desktop, or `poweroff` in a guest terminal.

The VM's disk lives in `./jmap-playground.qcow2` in your working directory and
**persists across boots**. Delete that file for a clean slate.

---

## Log in and test (inside the VM window)

A Matrix client (**nheko**) opens automatically. There's also a
**`HOW-TO-TEST.txt`** on the desktop with these same details. Log in with:

| Field | Value |
| --- | --- |
| Homeserver | `http://localhost:8008` |
| Username | `you` |
| Password | `playground` |

The account is **already registered** and the bridge is **already logged in** to
the mail server on your behalf — no registration token, no `login` step. After
signing in you'll have invites from `@_jmap_bot`:

- a **control room**, and
- **"Alice Tester (alice@example.com)"** — created from a seeded inbound email.

Accept the Alice room and type into it — that is a real reply, sent as email in
the same thread.

> Prefer Element? Open a terminal in the desktop and run `element-desktop`.
> Want to drive it from your **host** instead of the VM window? The Matrix
> (`localhost:8008`) and JMAP (`localhost:8081`) ports are also forwarded to the
> host, so a host client / `curl` works too.

### Ports forwarded to the host

| Host port | Service |
| --- | --- |
| `localhost:8008` | Matrix Client-Server API |
| `localhost:8081` | Stalwart JMAP |

---

## Things to try

1. **Reply to Alice.** Open the **Alice Tester** room and type (e.g.
   `hello back`). It goes out as email in that thread — see below for watching it
   land.
2. **Trash or junk a thread.** React to any message with **🗑** (move the whole
   thread to Trash and unbridge the room) or **🚫** (move it to Junk). The text
   commands `delete-room` and `spam` do the same thing
   ([ADR-0011](../../docs/adr/0011-command-emoji-duality.md)).
3. **Load remote images.** On an HTML mail, react **🖼️** — or reply to the
   message with `show-images`.
4. **Inject more mail** and watch new rooms appear — see the `curl` at the bottom.
5. **Message the bot** in the control room: `help`, `status`, `signature <text>`.

> The `⏳ → ✅` send-state reactions are **not** part of this: the send-delay hold
> window they belong to is off by default while the feature is finished
> ([ADR-0012](../../docs/adr/0012-matrix-actions-replicate-as-reversible-moves.md)),
> so replies here send immediately and unmarked. A permanent delivery failure
> still produces **❌** plus a notice.

### It really sends

Your reply is a genuine outbound send, delivered end-to-end **inside the VM**:
the bridge submits it against `bridgeuser@example.com`'s sending identity and
Stalwart delivers it locally to the contact `alice@example.com`, so the message
actually lands in Alice's mailbox — no external network involved. Watch it
arrive:

```bash
# From the host (or a guest terminal): read the contact's Inbox.
curl -sS -u alice:alicepass http://localhost:8081/jmap/session | jq .
```

Three things make this work (all in `stalwart-provision`, no bridge changes):

1. a **real dotted domain** (`example.com`) — Stalwart rejects `localhost` and
   reserved TLDs like `.test` as an *"Invalid e-mail address"* when creating an
   identity, so those silently break send;
2. an explicit **`Identity/set`** for the bridge account — Stalwart never
   auto-creates identities, and the bridge binds every `EmailSubmission` to
   whatever `Identity/get` returns;
3. a **local recipient** (`alice@example.com`) so delivery is loopback.

### If a send doesn't arrive

Watch the bridge logs live while you send (see below). The submit worker
([`src/retry.rs`](../../src/retry.rs)) resolves the recipient from the room's
ghost mapping; a fresh/unmapped room, or a rejected JMAP submission, is where
failures show up. The logs print `Sending fresh email…` /
`Sending ghost room reply…` and either `Submitted outbound message N` (success)
or the failure + `adding to retry queue`. After ten failed attempts the bridge
gives up, reacts **❌** and posts a permanent-failure notice.

---

## Peek under the hood (from inside the VM console)

```sh
# Live bridge logs
journalctl -u jmap-bridge -f

# The bridge's SQLite state — inspect the outbound queue
sqlite3 /var/lib/jmap-bridge/bridge.db 'SELECT id,event_id,release_at,retry_count FROM outbound_queue;'
sqlite3 /var/lib/jmap-bridge/bridge.db 'SELECT ghost_email,matrix_room_id FROM room_ghost_mapping;'

# Other services
journalctl -u stalwart -f
journalctl -u tuwunel -f
journalctl -u stalwart-provision      # account seeding (runs once at boot)
```

From the **host** you can also drive JMAP directly (account `bridgeuser` /
`bridgepass`) to inject more inbound email and watch new rooms appear:

```bash
curl -sS -u bridgeuser:bridgepass http://localhost:8081/jmap/session | jq .
```

---

## How it's wired

Everything lives in [`default.nix`](./default.nix):

- reuses the shared NixOS module [`nix/module`](../module) via `services.jmap-bridge`,
- `stalwart-provision.service` creates the mail domain, the bridge account **and
  its sending identity**, a local contact account (`alice@example.com`), and
  seeds one inbound email — ordered **before** the bridge so its declarative
  login finds a live mailbox,
- `virtualisation.vmVariant.virtualisation.forwardPorts` publishes 8008/8081.

If you change bridge source, just re-run `nix run .#playground` — it rebuilds the
package and boots the new binary against the persisted VM disk.
