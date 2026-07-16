# Playground VM

A **disposable local sandbox** that boots the whole bridge stack in one QEMU VM
**with a graphical desktop and a Matrix client already open**, so you can click
through the bridge by hand:

- **Stalwart** — a real JMAP mail server (the bridge's "email" side)
- **tuwunel** — a real Matrix homeserver (the bridge's "chat" side)
- **the bridge** — built from *this* checkout, wired to both
- **XFCE + nheko** — a lightweight desktop that auto-opens a Matrix client

Watch the send-delay `⏳ → ✅` reactions, edit/redact a held message, trip the
`❌` failure path, and so on. This is the interactive counterpart to the headless
round-trip test in [`nix/check`](../check); it reuses the same service wiring.

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

Accept the Alice room, reply in it, and watch the send-delay flow:
`⏳` held 5s → **redact** to cancel / **edit** to rewrite → `✅` sent / `❌` failed.

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

## Reproduce the send-delay flow (the `⏳ → ✅ / ❌` you asked about)

1. Open the **Alice Tester** room and send a reply (e.g. `hello back`).
2. The bridge reacts **⏳** to your message and, the first time per room, posts
   the explainer: *"Your messages are held 5s before sending…"*.
3. Within the 5-second window you can:
   - **redact** your message → the queued send is **cancelled** (nothing sent),
   - **edit** your message → the queued body is **rewritten**.
4. After 5s the worker submits it to Stalwart and the **⏳ is replaced by ✅**
   (submitted) — or **❌** if delivery permanently fails.

Change the window at any time by messaging the bridge `send-delay 10`
(seconds), or `send-delay off`.

### It really sends (you get a `✅`)

Your reply is a genuine outbound send, delivered end-to-end **inside the VM**:
the bridge submits it against `bridgeuser@example.com`'s sending identity and
Stalwart delivers it locally to the contact `alice@example.com`. So the held
`⏳` resolves to `✅`, and the message actually lands in Alice's mailbox — no
external network involved. Watch it arrive:

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

### If a send *doesn't* resolve to `✅`

Watch the bridge logs live while you send (see below). The submit worker
([`src/retry.rs`](../../src/retry.rs)) resolves the recipient from the room's
ghost mapping; a fresh/unmapped room, or a rejected JMAP submission, is where
failures show up. The logs print `Sending fresh email…` /
`Sending ghost room reply…` and either `Submitted outbound message N` (the `✅`)
or the failure + `adding to retry queue` (the `❌`/retry path).

---

## Peek under the hood (from inside the VM console)

```sh
# Live bridge logs
journalctl -u jmap-bridge -f

# The bridge's SQLite state — inspect the outbound send-delay queue
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
